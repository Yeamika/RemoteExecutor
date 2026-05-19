use crate::{
    apply_diffy, apply_patch, exbash, glob_paths, grep_paths, read_path, rg_search, ApplyOptions,
    DiffOptions, ExbashOptions, GlobOptions, GrepOptions, ReadOptions, RgOptions, ToolContext,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

#[derive(Clone, Debug, Deserialize)]
pub struct StdioRequest {
    pub id: Value,
    #[serde(alias = "tool")]
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub directory: Option<std::path::PathBuf>,
    #[serde(default)]
    pub worktree: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StdioResponse {
    pub id: Value,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl StdioResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}

pub async fn run_stdio() -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = BufWriter::new(tokio::io::stdout());

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<StdioRequest>(&line) {
            Ok(request) => handle_request(request).await,
            Err(err) => StdioResponse::err(Value::Null, format!("invalid request: {err}")),
        };

        let text = serde_json::to_string(&response)?;
        stdout.write_all(text.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

pub async fn handle_request(request: StdioRequest) -> StdioResponse {
    let id = request.id.clone();
    let method = request.method.as_str();
    let ctx = ToolContext::new(request.directory, request.worktree);

    match method {
        "exbash" | "exec" => match serde_json::from_value::<ExbashOptions>(request.params) {
            Ok(options) => match exbash(options, &ctx).await {
                Ok(output) => StdioResponse::ok(id, serde_json::json!(output)),
                Err(err) => StdioResponse::err(id, err.to_string()),
            },
            Err(err) => StdioResponse::err(id, format!("bad exbash params: {err}")),
        },
        "glob" => match serde_json::from_value::<GlobOptions>(request.params) {
            Ok(options) => match glob_paths(options, &ctx) {
                Ok(output) => StdioResponse::ok(id, serde_json::json!(output)),
                Err(err) => StdioResponse::err(id, err.to_string()),
            },
            Err(err) => StdioResponse::err(id, format!("bad glob params: {err}")),
        },
        "grep" => match serde_json::from_value::<GrepOptions>(request.params) {
            Ok(options) => match grep_paths(options, &ctx).await {
                Ok(output) => StdioResponse::ok(id, serde_json::json!(output)),
                Err(err) => StdioResponse::err(id, err.to_string()),
            },
            Err(err) => StdioResponse::err(id, format!("bad grep params: {err}")),
        },
        "read" => match serde_json::from_value::<ReadOptions>(request.params) {
            Ok(options) => match read_path(options, &ctx) {
                Ok(output) => StdioResponse::ok(id, serde_json::json!(output)),
                Err(err) => StdioResponse::err(id, err.to_string()),
            },
            Err(err) => StdioResponse::err(id, format!("bad read params: {err}")),
        },
        "diffy" | "apply_diff" => match serde_json::from_value::<DiffOptions>(request.params) {
            Ok(options) => match apply_diffy(options, &ctx).await {
                Ok(output) => StdioResponse::ok(id, serde_json::json!(output)),
                Err(err) => StdioResponse::err(id, err.to_string()),
            },
            Err(err) => StdioResponse::err(id, format!("bad diffy params: {err}")),
        },
        "apply" | "apply_patch" => match serde_json::from_value::<ApplyOptions>(request.params) {
            Ok(options) => match apply_patch(options, &ctx).await {
                Ok(output) => StdioResponse::ok(id, serde_json::json!(output)),
                Err(err) => StdioResponse::err(id, err.to_string()),
            },
            Err(err) => StdioResponse::err(id, format!("bad apply params: {err}")),
        },
        "rg" => match serde_json::from_value::<RgOptions>(request.params) {
            Ok(options) => match rg_search(options).await {
                Ok(output) => StdioResponse::ok(id, serde_json::json!(output)),
                Err(err) => StdioResponse::err(id, err.to_string()),
            },
            Err(err) => StdioResponse::err(id, format!("bad rg params: {err}")),
        },
        _ => StdioResponse::err(id, format!("unknown method: {method}")),
    }
}
