mod mcp;

use crate::{
    start_executor_ws, Executor, ExecutorInfo, ExecutorRequest, ExecutorResponse, ToolResult,
};
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

pub type StdioRequest = ExecutorRequest;
pub type StdioResponse = ExecutorResponse;

pub use mcp::{handle_mcp_message, run_mcp_stdio, run_mcp_stdio_with_caller};

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
    #[serde(alias = "executor")]
    pub id: String,
}

#[derive(Clone)]
pub struct Caller {
    state: Arc<Mutex<CallerState>>,
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
        let local = Executor::local("local");
        let local_info = local.info().clone();
        let local_addr = start_executor_ws("127.0.0.1:0", local)?;
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
        })
    }

    pub async fn handle(&self, request: ExecutorRequest) -> ExecutorResponse {
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
        request.executor = None;
        match call_ws(&self.url, request).await {
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
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = BufWriter::new(tokio::io::stdout());

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<StdioRequest>(&line) {
            Ok(request) => caller.handle(request).await,
            Err(err) => ExecutorResponse::err(
                Value::Null,
                Some("caller".to_string()),
                format!("invalid request: {err}"),
            ),
        };

        let text = serde_json::to_string(&response)?;
        stdout.write_all(text.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

pub async fn handle_request(request: StdioRequest) -> StdioResponse {
    match Caller::new().await {
        Ok(caller) => caller.handle(request).await,
        Err(err) => ExecutorResponse::err(request.id, Some("caller".to_string()), err.to_string()),
    }
}

async fn call_ws(url: &str, request: ExecutorRequest) -> Result<ExecutorResponse> {
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
    matches!(
        method,
        "list_executor" | "list_executors" | "listexecutor" | "listexecutors" | "listexecuotr"
    )
}

fn is_connect_executor(method: &str) -> bool {
    matches!(
        method,
        "connect_to_executor" | "connectto_executor" | "connecttoexecutor" | "connect_executor"
    )
}

fn is_set_default_executor(method: &str) -> bool {
    matches!(
        method,
        "set_default_executor" | "set_def_executor" | "setdefexecutor" | "set_defexecutor"
    )
}
