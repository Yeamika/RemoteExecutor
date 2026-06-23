mod dispatch;
#[cfg(test)]
mod test;
mod ws;

use crate::{
    exbash, file_action, glob_paths, handle_file_transfer_http, is_file_transfer_http_request,
    list_shells, read_path, request_reload, rg_search, set_default_shell, stat_path, tool_output,
    ExbashOptions, ExecutorInfo, ExecutorRequest, ExecutorResponse, FileActionOptions,
    FileTransferState, GlobOptions, ListShellsOptions, ReadOptions, RequestReloadOptions,
    RgOptions, SetDefaultShellOptions, SettingsStore, ShellManager, StatOptions, ToolContext,
    ToolResult,
};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Number, Value};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_TOOL_TIMEOUT_MS: u64 = 5_000;
const MAX_TOOL_TIMEOUT_MS: u64 = 600_000;

#[derive(Clone)]
pub struct Executor {
    info: ExecutorInfo,
    shell_manager: Option<ShellManager>,
    settings_store: SettingsStore,
    directory_settings: Arc<StdMutex<BTreeMap<PathBuf, SettingsStore>>>,
    workspace_settings: bool,
    write_lock: Arc<AsyncMutex<()>>,
}

impl Executor {
    pub fn new(info: ExecutorInfo) -> Self {
        Self {
            info,
            shell_manager: None,
            settings_store: SettingsStore::load_default_lossy(),
            directory_settings: Arc::new(StdMutex::new(BTreeMap::new())),
            workspace_settings: false,
            write_lock: Arc::new(AsyncMutex::new(())),
        }
    }
    pub fn local(id: impl Into<String>) -> Self {
        Self::new(ExecutorInfo {
            id: id.into(),
            system: Some(std::env::consts::OS.to_string()),
            device: std::env::var("HOSTNAME").ok(),
            labels: BTreeMap::new(),
        })
        .with_shell_manager(ShellManager::default_shell(80, 24))
        .with_workspace_settings()
    }

    pub fn info(&self) -> &ExecutorInfo {
        &self.info
    }

    pub fn with_shell_manager(mut self, shell_manager: ShellManager) -> Self {
        self.shell_manager = Some(shell_manager);
        self
    }

    pub fn with_settings_store(mut self, settings_store: SettingsStore) -> Self {
        self.settings_store = settings_store;
        self.directory_settings = Arc::new(StdMutex::new(BTreeMap::new()));
        self
    }

    pub fn with_workspace_settings(mut self) -> Self {
        self.workspace_settings = true;
        self
    }

    pub async fn handle(&self, request: ExecutorRequest) -> ExecutorResponse {
        let id = request.id.clone();
        let method = request.method.clone();
        if method == "_executor_info" {
            return ExecutorResponse::ok(
                id,
                Some(self.info.id.clone()),
                serde_json::json!(self.executor_info_result()),
            );
        }
        let directory = request.directory.clone();
        let timeout_ms = effective_tool_timeout_ms(request.tool_timeout_ms);
        let params = apply_soft_timeout_param(&method, request.params, request.tool_timeout_ms);
        let settings_store = match self.settings_for_request(directory.as_deref()) {
            Ok(settings) => settings,
            Err(err) => {
                return ExecutorResponse::err(id, Some(self.info.id.clone()), err.to_string())
            }
        };
        let mut ctx = ToolContext::new(directory).with_settings_store(settings_store);
        if let Some(shell_manager) = &self.shell_manager {
            ctx = ctx.with_shell_manager(shell_manager.clone());
        }
        let result = if is_write_method(&method) {
            let _guard = self.write_lock.lock().await;
            self.dispatch_with_timeout(&method, params, &ctx, timeout_ms)
                .await
        } else {
            self.dispatch_with_timeout(&method, params, &ctx, timeout_ms)
                .await
        };

        match result {
            Ok(output) => {
                ExecutorResponse::ok(id, Some(self.info.id.clone()), serde_json::json!(output))
            }
            Err(err) => ExecutorResponse::err(id, Some(self.info.id.clone()), err.to_string()),
        }
    }

    async fn dispatch_with_timeout(
        &self,
        method: &str,
        params: Value,
        ctx: &ToolContext,
        timeout_ms: u64,
    ) -> Result<ToolResult> {
        if is_exbash_method(method) || is_soft_timeout_method(method) {
            return dispatch_tool(method, params, ctx).await;
        }
        match timeout(
            Duration::from_millis(timeout_ms),
            dispatch_tool(method, params, ctx),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "tool {method} timed out after {timeout_ms}ms"
            )),
        }
    }

    fn executor_info_result(&self) -> ToolResult {
        let value = serde_json::json!({
            "id": self.info.id,
            "protocol": "remote-executor",
            "version": env!("CARGO_PKG_VERSION"),
            "system": self.info.system,
            "device": self.info.device,
            "labels": self.info.labels,
            "capabilities": {
                "fileTransfer": true,
            },
            "fileTransferPath": "/re-file/v1",
        });
        ToolResult {
            metadata: value.clone(),
            output: tool_output(serde_json::to_string_pretty(&value).unwrap_or_default()),
        }
    }

    fn settings_for_request(&self, directory: Option<&Path>) -> Result<SettingsStore> {
        if self.workspace_settings {
            if let Some(directory) = directory {
                return self.settings_for_directory(directory);
            }
        }
        Ok(self.settings_store.clone())
    }

    fn settings_for_directory(&self, directory: &Path) -> Result<SettingsStore> {
        let key = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            std::env::current_dir()?.join(directory)
        };
        if let Some(settings) = self.directory_settings.lock().unwrap().get(&key).cloned() {
            return Ok(settings);
        }
        let settings = self.settings_store.for_directory(&key)?;
        self.directory_settings
            .lock()
            .unwrap()
            .insert(key, settings.clone());
        Ok(settings)
    }
}

pub fn start_shared_executor_ws(
    addr: impl Into<String>,
    executor: Executor,
    manager: crate::ShellManager,
) -> Result<String> {
    let addr = addr.into();
    let std_listener = std::net::TcpListener::bind(&addr)?;
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;
    let actual_addr = listener.local_addr()?.to_string();
    let file_state = FileTransferState::new(executor.write_lock.clone());
    let executor = executor.with_shell_manager(manager.clone());

    tokio::spawn(async move {
        while let Ok((stream, peer_addr)) = listener.accept().await {
            let executor = executor.clone();
            let manager = manager.clone();
            let file_state = file_state.clone();
            tokio::spawn(async move {
                if let Err(err) =
                    handle_shared_connection(stream, peer_addr, executor, manager, file_state).await
                {
                    if is_disconnect_error(&err) {
                        return;
                    }
                    eprintln!("shared executor websocket error: {err:#}");
                }
            });
        }
    });

    Ok(actual_addr)
}

async fn handle_shared_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    executor: Executor,
    manager: crate::ShellManager,
    file_state: FileTransferState,
) -> Result<()> {
    if is_file_transfer_http_request(&stream).await? {
        return handle_file_transfer_http(stream, file_state).await;
    }
    handle_shared_ws(stream, peer_addr, executor, manager).await
}

async fn handle_shared_ws(
    stream: TcpStream,
    peer_addr: SocketAddr,
    executor: Executor,
    manager: crate::ShellManager,
) -> Result<()> {
    let ws = accept_async(stream).await?;
    let (mut write, mut read) = ws.split();
    let Some(first) = read.next().await else {
        return Ok(());
    };
    let first = first?;
    let first_text = first.into_text()?;

    if serde_json::from_str::<ExecutorRequest>(&first_text).is_ok() {
        send_executor_response(&mut write, &executor, &first_text).await?;
        while let Some(message) = read.next().await {
            match message? {
                Message::Text(text) => send_executor_response(&mut write, &executor, &text).await?,
                Message::Ping(data) => write.send(Message::Pong(data)).await?,
                Message::Close(_) => break,
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        return Ok(());
    }

    crate::websocket::handle_first_text(first_text.to_string(), write, read, peer_addr, manager)
        .await
}

async fn send_executor_response(
    write: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        Message,
    >,
    executor: &Executor,
    text: &str,
) -> Result<()> {
    let response = match serde_json::from_str::<ExecutorRequest>(text) {
        Ok(request) => executor.handle(request).await,
        Err(err) => ExecutorResponse::err(Value::Null, None, format!("invalid request: {err}")),
    };
    write
        .send(Message::Text(serde_json::to_string(&response)?.into()))
        .await?;
    Ok(())
}

fn is_disconnect_error(err: &anyhow::Error) -> bool {
    let text = err.to_string();
    text.contains("Connection reset without closing handshake")
        || text.contains("connection reset by peer")
        || text.contains("Broken pipe")
}

fn effective_tool_timeout_ms(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_MS)
        .min(MAX_TOOL_TIMEOUT_MS)
}

fn is_exbash_method(method: &str) -> bool {
    method == "exbash"
}

fn is_write_method(method: &str) -> bool {
    matches!(method, "FileAction" | "set_default_shell")
}

fn is_soft_timeout_method(method: &str) -> bool {
    matches!(method, "rg" | "glob")
}

fn apply_soft_timeout_param(method: &str, mut params: Value, requested: Option<u64>) -> Value {
    if !is_soft_timeout_method(method) {
        return params;
    }
    let Some(timeout_ms) = requested else {
        return params;
    };
    if let Some(object) = params.as_object_mut() {
        object
            .entry("timeout")
            .or_insert_with(|| Value::Number(Number::from(timeout_ms)));
    }
    params
}

pub async fn dispatch_tool(method: &str, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
    match method {
        "exbash" => exbash(serde_json::from_value::<ExbashOptions>(params)?, ctx).await,
        "read" => read_path(serde_json::from_value::<ReadOptions>(params)?, ctx),
        "stat" => stat_path(serde_json::from_value::<StatOptions>(params)?, ctx),
        "FileAction" => {
            file_action(serde_json::from_value::<FileActionOptions>(params)?, ctx).await
        }
        "glob" => glob_paths(serde_json::from_value::<GlobOptions>(params)?, ctx),
        "set_default_shell" => set_default_shell(
            serde_json::from_value::<SetDefaultShellOptions>(params)?,
            ctx,
        ),
        "list_shells" => list_shells(serde_json::from_value::<ListShellsOptions>(params)?, ctx),
        "request_reload" => {
            request_reload(serde_json::from_value::<RequestReloadOptions>(params)?, ctx)
        }
        "rg" => {
            let output = rg_search(serde_json::from_value::<RgOptions>(params)?).await?;
            Ok(ToolResult {
                metadata: serde_json::json!({
                    "matches": output.matches,
                    "filesWalked": output.files_walked,
                    "code": output.code,
                    "timedOut": output.timed_out
                }),
                output: tool_output(output.stdout),
            })
        }
        _ => Err(anyhow::anyhow!("unknown method: {method}")),
    }
}
