mod mcp;
mod state;
mod stdio;

#[cfg(test)]
mod mcp_test;
#[cfg(test)]
mod stdio_test;
#[cfg(test)]
mod test;

use crate::{
    exbash_run_detail, start_shared_executor_ws, tool_output, Executor, ExecutorInfo,
    ExecutorRequest, ExecutorResponse, SettingsStore, ShellManager, ToolResult,
};
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use pty_t_core::TermSize;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_CALL_TIMEOUT_MS: u64 = 30_000;
const EXBASH_TIMEOUT_BUFFER_MS: u64 = 5_000;

pub type StdioRequest = ExecutorRequest;
pub type StdioResponse = ExecutorResponse;

pub use mcp::{
    handle_mcp_message, run_mcp_stdio, run_mcp_stdio_io_with_caller, run_mcp_stdio_with_caller,
    run_mcp_stdio_with_settings_path,
};

#[derive(Clone, Debug, Deserialize)]
pub struct ConnectExecutorOptions {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SetDefaultExecutorOptions {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FileTransferOptions {
    pub mode: FileTransferMode,
    #[serde(rename = "localPath")]
    pub local_path: PathBuf,
    #[serde(rename = "targetPath")]
    pub target_path: PathBuf,
    #[serde(default)]
    pub overwrite: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileTransferMode {
    Download,
    Upload,
}

#[derive(Clone)]
pub struct Caller {
    state: Arc<Mutex<CallerState>>,
    write_lock: Arc<Mutex<()>>,
    local_shell_manager: ShellManager,
}

#[derive(Clone)]
struct CallerState {
    default_executor: String,
    executors: BTreeMap<String, ExecutorEndpoint>,
}

#[derive(Clone)]
struct ExecutorEndpoint {
    info: ExecutorInfo,
    url: String,
    protocol: Option<String>,
    version: Option<String>,
    file_transfer: bool,
    file_transfer_path: String,
}

impl Caller {
    pub async fn new() -> Result<Self> {
        Self::new_with_settings(None).await
    }

    pub async fn new_with_settings(settings_path: Option<std::path::PathBuf>) -> Result<Self> {
        let settings = SettingsStore::load(settings_path)?;
        Self::new_with_settings_store(settings).await
    }

    pub async fn new_with_settings_store(settings: SettingsStore) -> Result<Self> {
        let shell_manager = ShellManager::new(
            settings.interactive_command_spec()?,
            TermSize { cols: 80, rows: 24 },
        );
        shell_manager.create_pty("main", settings.interactive_command_spec()?, None, None)?;
        let local = Executor::local("local")
            .with_shell_manager(shell_manager.clone())
            .with_settings_store(settings);
        let local_info = local.info().clone();
        let local_addr =
            start_shared_executor_ws("127.0.0.1:0", local.clone(), shell_manager.clone())?;
        let local_endpoint = ExecutorEndpoint {
            info: local_info,
            url: format!("ws://{local_addr}"),
            protocol: Some("remote-executor".to_string()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            file_transfer: true,
            file_transfer_path: "/re-file/v1".to_string(),
        };
        let mut executors = BTreeMap::new();
        executors.insert("local".to_string(), local_endpoint);

        Ok(Self {
            state: Arc::new(Mutex::new(CallerState {
                default_executor: "local".to_string(),
                executors,
            })),
            write_lock: Arc::new(Mutex::new(())),
            local_shell_manager: shell_manager,
        })
    }

    pub fn subscribe_local_exit_code(
        &self,
        async_id: &str,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<u32>> {
        self.local_shell_manager.subscribe_exit_code(async_id)
    }

    pub fn local_exbash_run_detail(&self, async_id: &str) -> Result<Value> {
        exbash_run_detail(&self.local_shell_manager, async_id)
    }

    pub async fn handle(&self, request: ExecutorRequest) -> ExecutorResponse {
        if is_write_method(&request.method) {
            let id = request.id.clone();
            let Ok(_guard) = self.write_lock.try_lock() else {
                return self.err(id, "another write operation is already running");
            };
            return self.handle_inner(request).await;
        }

        self.handle_inner(request).await
    }

    async fn handle_inner(&self, request: ExecutorRequest) -> ExecutorResponse {
        if is_list_executor(&request.method) {
            return self.ok(request.id, self.list_executor_result().await);
        }
        if is_connect_executor(&request.method) {
            return self.connect_executor_response(request).await;
        }
        if is_set_default_executor(&request.method) {
            return self.set_default_response(request).await;
        }
        if is_file_transfer(&request.method) {
            return self.file_transfer_response(request).await;
        }

        let request_id = request.id.clone();
        let selected = match self.select_executor(request.executor.as_deref()).await {
            Ok(endpoint) => endpoint,
            Err(err) => return self.err(request_id, err.to_string()),
        };
        selected.call(request).await
    }

    pub async fn connect_to_executor(&self, options: ConnectExecutorOptions) -> Result<()> {
        if options.id.trim().is_empty() {
            return Err(anyhow!("executor id is required"));
        }
        if options.id == "local" {
            return Err(anyhow!("local executor is reserved"));
        }

        let mut endpoint = ExecutorEndpoint {
            info: ExecutorInfo {
                id: options.id.clone(),
                system: options.system,
                device: options.device,
                labels: options.labels,
            },
            url: normalize_ws_url(&options.url),
            protocol: None,
            version: None,
            file_transfer: false,
            file_transfer_path: "/re-file/v1".to_string(),
        };
        endpoint.refresh_info().await?;
        self.state
            .lock()
            .await
            .executors
            .insert(options.id, endpoint);
        Ok(())
    }

    pub async fn set_default_executor(&self, id: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        if !state.executors.contains_key(id) {
            return Err(anyhow!("executor not found: {id}"));
        }
        state.default_executor = id.to_string();
        Ok(())
    }

    pub async fn default_executor(&self) -> String {
        self.state.lock().await.default_executor.clone()
    }

    async fn select_executor(&self, requested: Option<&str>) -> Result<ExecutorEndpoint> {
        let state = self.state.lock().await;
        let id = requested.unwrap_or(&state.default_executor);
        state
            .executors
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("executor not found: {id}"))
    }

    async fn list_executor_result(&self) -> ToolResult {
        let state = self.state.lock().await;
        let executors = state
            .executors
            .values()
            .map(|endpoint| {
                json!({
                    "id": endpoint.info.id,
                    "system": endpoint.info.system,
                    "device": endpoint.info.device,
                    "labels": endpoint.info.labels,
                    "url": endpoint.url,
                    "protocol": endpoint.protocol,
                    "version": endpoint.version,
                    "fileTransfer": endpoint.file_transfer,
                    "fileTransferPath": endpoint.file_transfer_path,
                })
            })
            .collect::<Vec<_>>();
        let value = json!({
            "default": state.default_executor,
            "executors": executors,
        });
        ToolResult {
            metadata: value.clone(),
            output: tool_output(serde_json::to_string_pretty(&value).unwrap_or_default()),
        }
    }

    async fn connect_executor_response(&self, request: ExecutorRequest) -> ExecutorResponse {
        let id = request.id.clone();
        match serde_json::from_value::<ConnectExecutorOptions>(request.params) {
            Ok(options) => match self.connect_to_executor(options).await {
                Ok(()) => self.ok(id, self.list_executor_result().await),
                Err(err) => self.err(id, err.to_string()),
            },
            Err(err) => self.err(id, format!("bad connect_to_executor params: {err}")),
        }
    }

    async fn set_default_response(&self, request: ExecutorRequest) -> ExecutorResponse {
        let id = request.id.clone();
        match serde_json::from_value::<SetDefaultExecutorOptions>(request.params) {
            Ok(options) => match self.set_default_executor(&options.id).await {
                Ok(()) => self.ok(id, self.list_executor_result().await),
                Err(err) => self.err(id, err.to_string()),
            },
            Err(err) => self.err(id, format!("bad set_default_executor params: {err}")),
        }
    }

    async fn file_transfer_response(&self, request: ExecutorRequest) -> ExecutorResponse {
        let id = request.id.clone();
        let options = match serde_json::from_value::<FileTransferOptions>(request.params) {
            Ok(options) => options,
            Err(err) => return self.err(id, format!("bad file_transfer params: {err}")),
        };
        let selected = match self.select_executor(request.executor.as_deref()).await {
            Ok(endpoint) => endpoint,
            Err(err) => return self.err(id, err.to_string()),
        };
        match selected.file_transfer_result(options, request.directory.as_deref()) {
            Ok(result) => self.ok(id, result),
            Err(err) => self.err(id, err.to_string()),
        }
    }

    fn ok(&self, id: Value, result: ToolResult) -> ExecutorResponse {
        ExecutorResponse::ok(id, Some("caller".to_string()), json!(result))
    }

    fn err(&self, id: Value, error: impl Into<String>) -> ExecutorResponse {
        ExecutorResponse::err(id, Some("caller".to_string()), error)
    }
}

impl ExecutorEndpoint {
    async fn refresh_info(&mut self) -> Result<()> {
        let request = ExecutorRequest {
            id: json!("_executor_info"),
            method: "_executor_info".to_string(),
            params: json!({}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        };
        let response = call_ws(&self.url, request, Some(DEFAULT_CALL_TIMEOUT_MS)).await?;
        if !response.ok {
            return Err(anyhow!(response
                .error
                .unwrap_or_else(|| "_executor_info failed".to_string())));
        }
        let result = response.result.unwrap_or(Value::Null);
        let metadata = result.get("metadata").unwrap_or(&result);
        let version = metadata
            .get("version")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("executor info missing version"))?;
        let capabilities = metadata
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("executor info missing capabilities"))?;
        self.protocol = metadata
            .get("protocol")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.version = Some(version.to_string());
        if self.info.system.is_none() {
            self.info.system = metadata
                .get("system")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if self.info.device.is_none() {
            self.info.device = metadata
                .get("device")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if self.info.labels.is_empty() {
            if let Some(labels) = metadata.get("labels").and_then(Value::as_object) {
                self.info.labels = labels
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_string()))
                    })
                    .collect();
            }
        }
        self.file_transfer = metadata
            .pointer("/capabilities/fileTransfer")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.file_transfer_path = metadata
            .get("fileTransferPath")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("/re-file/v1")
            .to_string();
        if self.file_transfer && !capabilities.contains_key("fileTransfer") {
            return Err(anyhow!("executor info missing fileTransfer capability"));
        }
        Ok(())
    }

    async fn call(&self, mut request: ExecutorRequest) -> ExecutorResponse {
        let request_id = request.id.clone();
        let call_timeout_ms = call_timeout_ms_for(&request);
        request.executor = None;
        match call_ws(&self.url, request, call_timeout_ms).await {
            Ok(mut response) => {
                response
                    .executor
                    .get_or_insert_with(|| self.info.id.clone());
                response
            }
            Err(err) => ExecutorResponse::err(
                request_id,
                Some(self.info.id.clone()),
                format!("executor {} call failed: {err}", self.info.id),
            ),
        }
    }

    fn file_transfer_result(
        &self,
        options: FileTransferOptions,
        directory: Option<&std::path::Path>,
    ) -> Result<ToolResult> {
        if !self.file_transfer {
            return Err(anyhow!(
                "executor {} does not advertise file transfer support",
                self.info.id
            ));
        }
        let url = http_file_transfer_url(&self.url)?;
        let method = match options.mode {
            FileTransferMode::Download => "GET",
            FileTransferMode::Upload => "PUT",
        };
        let mut headers = serde_json::Map::new();
        headers.insert(
            "X-RE-Path".to_string(),
            Value::String(options.target_path.to_string_lossy().into_owned()),
        );
        if let Some(directory) = directory {
            headers.insert(
                "X-RE-Directory".to_string(),
                Value::String(directory.to_string_lossy().into_owned()),
            );
        }
        if let Some(overwrite) = options.overwrite {
            headers.insert(
                "X-RE-Overwrite".to_string(),
                Value::String(overwrite.to_string()),
            );
        }
        let metadata = json!({
            "executor": self.info.id,
            "mode": match method {
                "GET" => "download",
                _ => "upload",
            },
            "localPath": options.local_path,
            "targetPath": options.target_path,
            "method": method,
            "url": url,
            "headers": headers,
        });
        let header_lines = metadata["headers"]
            .as_object()
            .into_iter()
            .flat_map(|headers| headers.iter())
            .map(|(key, value)| format!("{key}: {}", value.as_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n");
        let text = if header_lines.is_empty() {
            format!("{method} {url}")
        } else {
            format!("{method} {url}\n{header_lines}")
        };
        Ok(ToolResult {
            metadata,
            output: tool_output(text),
        })
    }
}

pub async fn run_stdio() -> Result<()> {
    run_stdio_with_settings_path(None).await
}

pub async fn run_stdio_with_settings_path(settings_path: Option<std::path::PathBuf>) -> Result<()> {
    let caller = Caller::new_with_settings(settings_path).await?;
    run_stdio_with_caller(caller).await
}

pub async fn run_stdio_with_caller(caller: Caller) -> Result<()> {
    run_stdio_io_with_caller(
        caller,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

pub async fn run_stdio_io_with_caller<R, W>(caller: Caller, reader: R, writer: W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = reader.lines();
    let stdout = Arc::new(Mutex::new(BufWriter::new(writer)));
    let mut tasks = JoinSet::new();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let caller = caller.clone();
        let stdout = stdout.clone();
        tasks.spawn(async move {
            let response = match serde_json::from_str::<StdioRequest>(&line) {
                Ok(request) => caller.handle(request).await,
                Err(err) => ExecutorResponse::err(
                    Value::Null,
                    Some("caller".to_string()),
                    format!("invalid request: {err}"),
                ),
            };
            write_stdio_response(stdout, response).await
        });

        while let Some(result) = tasks.try_join_next() {
            result??;
        }
    }

    while let Some(result) = tasks.join_next().await {
        result??;
    }

    Ok(())
}

async fn write_stdio_response<W>(
    stdout: Arc<Mutex<BufWriter<W>>>,
    response: StdioResponse,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let text = serde_json::to_string(&response)?;
    let mut stdout = stdout.lock().await;
    stdout.write_all(text.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

pub async fn handle_request(request: StdioRequest) -> StdioResponse {
    match Caller::new().await {
        Ok(caller) => caller.handle(request).await,
        Err(err) => ExecutorResponse::err(request.id, Some("caller".to_string()), err.to_string()),
    }
}

async fn call_ws(
    url: &str,
    request: ExecutorRequest,
    call_timeout_ms: Option<u64>,
) -> Result<ExecutorResponse> {
    let request_id = request.id.clone();
    if let Some(call_timeout_ms) = call_timeout_ms {
        match timeout(
            Duration::from_millis(call_timeout_ms),
            call_ws_inner(url, request),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "executor call timed out after {call_timeout_ms}ms for request {request_id}"
            )),
        }
    } else {
        call_ws_inner(url, request).await
    }
}

async fn call_ws_inner(url: &str, request: ExecutorRequest) -> Result<ExecutorResponse> {
    let request_id = request.id.clone();
    let (ws, _) = connect_async(url).await?;
    let (mut write, mut read) = ws.split();
    write
        .send(Message::Text(serde_json::to_string(&request)?.into()))
        .await?;

    while let Some(message) = read.next().await {
        match message? {
            Message::Text(text) => return Ok(serde_json::from_str::<ExecutorResponse>(&text)?),
            Message::Ping(data) => write.send(Message::Pong(data)).await?,
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }

    Err(anyhow!(
        "executor closed before responding to request {request_id}"
    ))
}

fn normalize_ws_url(url: &str) -> String {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        url.to_string()
    } else {
        format!("ws://{url}")
    }
}

fn http_file_transfer_url(control_url: &str) -> Result<String> {
    if let Some(rest) = control_url.strip_prefix("ws://") {
        return Ok(format!("http://{rest}/re-file/v1"));
    }
    if let Some(rest) = control_url.strip_prefix("wss://") {
        return Ok(format!("https://{rest}/re-file/v1"));
    }
    Err(anyhow!(
        "unsupported executor URL for file transfer: {control_url}"
    ))
}

fn call_timeout_ms_for(request: &ExecutorRequest) -> Option<u64> {
    if request.method == "exbash" {
        let read_timeout = request
            .params
            .get("read_timeout")
            .and_then(Value::as_u64)
            .unwrap_or(10_000);
        return Some(read_timeout.saturating_add(EXBASH_TIMEOUT_BUFFER_MS));
    }
    if matches!(request.method.as_str(), "rg" | "glob") {
        let search_timeout = request
            .params
            .get("timeout")
            .and_then(Value::as_i64)
            .unwrap_or(10_000);
        if search_timeout == -1 {
            return None;
        }
        return Some((search_timeout.max(0) as u64).saturating_add(EXBASH_TIMEOUT_BUFFER_MS));
    }

    Some(DEFAULT_CALL_TIMEOUT_MS)
}

fn is_list_executor(method: &str) -> bool {
    method == "list_executor"
}

fn is_connect_executor(method: &str) -> bool {
    method == "connect_to_executor"
}

fn is_set_default_executor(method: &str) -> bool {
    method == "set_default_executor"
}

fn is_file_transfer(method: &str) -> bool {
    method == "file_transfer"
}

fn is_write_method(method: &str) -> bool {
    matches!(method, "FileAction" | "set_default_shell")
}
