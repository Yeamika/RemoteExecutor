use crate::{
    apply_diffy, apply_patch, exbash, glob_paths, grep_paths, read_path, rg_search, ApplyOptions,
    DiffOptions, ExbashOptions, GlobOptions, GrepOptions, ReadOptions, RgOptions, ToolContext,
    ToolResult,
};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorRequest {
    pub id: Value,
    #[serde(alias = "tool")]
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorResponse {
    pub id: Value,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
}

impl ExecutorResponse {
    pub fn ok(id: Value, executor: Option<String>, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
            executor,
        }
    }

    pub fn err(id: Value, executor: Option<String>, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.into()),
            executor,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Executor {
    info: ExecutorInfo,
}

impl Executor {
    pub fn new(info: ExecutorInfo) -> Self {
        Self { info }
    }

    pub fn local(id: impl Into<String>) -> Self {
        Self::new(ExecutorInfo {
            id: id.into(),
            system: Some(std::env::consts::OS.to_string()),
            device: std::env::var("HOSTNAME").ok(),
            labels: BTreeMap::new(),
        })
    }

    pub fn info(&self) -> &ExecutorInfo {
        &self.info
    }

    pub async fn handle(&self, request: ExecutorRequest) -> ExecutorResponse {
        let id = request.id.clone();
        let ctx = ToolContext::new(request.directory, request.worktree);
        match dispatch_tool(&request.method, request.params, &ctx).await {
            Ok(output) => {
                ExecutorResponse::ok(id, Some(self.info.id.clone()), serde_json::json!(output))
            }
            Err(err) => ExecutorResponse::err(id, Some(self.info.id.clone()), err.to_string()),
        }
    }
}

pub fn start_executor_ws(addr: impl Into<String>, executor: Executor) -> Result<String> {
    let addr = addr.into();
    let std_listener = std::net::TcpListener::bind(&addr)?;
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;
    let actual_addr = listener.local_addr()?.to_string();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let executor = executor.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_executor_ws(stream, executor).await {
                    if is_disconnect_error(&err) {
                        return;
                    }
                    eprintln!("executor websocket error: {err:#}");
                }
            });
        }
    });

    Ok(actual_addr)
}

async fn handle_executor_ws(stream: TcpStream, executor: Executor) -> Result<()> {
    let ws = accept_async(stream).await?;
    let (mut write, mut read) = ws.split();

    while let Some(message) = read.next().await {
        match message? {
            Message::Text(text) => {
                let response = match serde_json::from_str::<ExecutorRequest>(&text) {
                    Ok(request) => executor.handle(request).await,
                    Err(err) => {
                        ExecutorResponse::err(Value::Null, None, format!("invalid request: {err}"))
                    }
                };
                write
                    .send(Message::Text(serde_json::to_string(&response)?.into()))
                    .await?;
            }
            Message::Ping(data) => write.send(Message::Pong(data)).await?,
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }

    Ok(())
}

fn is_disconnect_error(err: &anyhow::Error) -> bool {
    let text = err.to_string();
    text.contains("Connection reset without closing handshake")
        || text.contains("connection reset by peer")
        || text.contains("Broken pipe")
}

pub async fn dispatch_tool(method: &str, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
    match method {
        "exbash" | "exec" => exbash(serde_json::from_value::<ExbashOptions>(params)?, ctx).await,
        "glob" => glob_paths(serde_json::from_value::<GlobOptions>(params)?, ctx),
        "grep" => grep_paths(serde_json::from_value::<GrepOptions>(params)?, ctx).await,
        "read" => read_path(serde_json::from_value::<ReadOptions>(params)?, ctx),
        "diffy" | "apply_diff" => {
            apply_diffy(serde_json::from_value::<DiffOptions>(params)?, ctx).await
        }
        "apply" | "apply_patch" => {
            apply_patch(serde_json::from_value::<ApplyOptions>(params)?, ctx).await
        }
        "rg" => {
            let output = rg_search(serde_json::from_value::<RgOptions>(params)?).await?;
            Ok(ToolResult {
                title: "rg".to_string(),
                metadata: serde_json::json!({ "matches": output.matches, "code": output.code }),
                output: output.stdout,
            })
        }
        _ => Err(anyhow::anyhow!("unknown method: {method}")),
    }
}
