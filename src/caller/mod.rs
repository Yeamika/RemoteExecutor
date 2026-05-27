mod mcp;

use crate::{
    start_shared_executor_ws, Executor, ExecutorInfo, ExecutorRequest, ExecutorResponse,
    ShellManager, ToolResult,
};
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
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

#[derive(Clone)]
pub struct Caller {
    state: Arc<Mutex<CallerState>>,
    write_lock: Arc<Mutex<()>>,
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
}

impl Caller {
    pub async fn new() -> Result<Self> {
        let shell_manager = ShellManager::default_shell(80, 24);
        shell_manager.create_bash("main")?;
        let local = Executor::local("local").with_shell_manager(shell_manager.clone());
        let local_info = local.info().clone();
        let local_addr = start_shared_executor_ws("127.0.0.1:0", local, shell_manager)?;
        let local_endpoint = ExecutorEndpoint {
            info: local_info,
            url: format!("ws://{local_addr}"),
        };
        let mut executors = BTreeMap::new();
        executors.insert("local".to_string(), local_endpoint);

        Ok(Self {
            state: Arc::new(Mutex::new(CallerState {
                default_executor: "local".to_string(),
                executors,
            })),
            write_lock: Arc::new(Mutex::new(())),
        })
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

        let endpoint = ExecutorEndpoint {
            info: ExecutorInfo {
                id: options.id.clone(),
                system: options.system,
                device: options.device,
                labels: options.labels,
            },
            url: normalize_ws_url(&options.url),
        };
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
                })
            })
            .collect::<Vec<_>>();
        let value = json!({
            "default": state.default_executor,
            "executors": executors,
        });
        ToolResult {
            title: "Executors listed".to_string(),
            metadata: value.clone(),
            output: serde_json::to_string_pretty(&value).unwrap_or_default(),
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

    fn ok(&self, id: Value, result: ToolResult) -> ExecutorResponse {
        ExecutorResponse::ok(id, Some("caller".to_string()), json!(result))
    }

    fn err(&self, id: Value, error: impl Into<String>) -> ExecutorResponse {
        ExecutorResponse::err(id, Some("caller".to_string()), error)
    }
}

impl ExecutorEndpoint {
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
}

pub async fn run_stdio() -> Result<()> {
    let caller = Caller::new().await?;
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
    call_timeout_ms: u64,
) -> Result<ExecutorResponse> {
    let request_id = request.id.clone();
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

fn is_list_executor(method: &str) -> bool {
    method == "list_executor"
}

fn is_connect_executor(method: &str) -> bool {
    method == "connect_to_executor"
}

fn is_set_default_executor(method: &str) -> bool {
    method == "set_default_executor"
}

fn is_write_method(method: &str) -> bool {
    matches!(method, "apply_patch" | "diffy")
}

fn call_timeout_ms_for(request: &ExecutorRequest) -> u64 {
    if matches!(
        request.method.as_str(),
        "exbash" | "exbash_shell" | "exbash_attach"
    ) {
        let read_timeout = request
            .params
            .get("read_timeout")
            .and_then(Value::as_u64)
            .unwrap_or(10_000);
        return read_timeout.saturating_add(EXBASH_TIMEOUT_BUFFER_MS);
    }

    DEFAULT_CALL_TIMEOUT_MS
}
