use super::Caller;
use crate::{ExecutorRequest, ExecutorResponse};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run_mcp_stdio() -> Result<()> {
    let caller = Caller::new().await?;
    run_mcp_stdio_with_caller(caller).await
}

pub async fn run_mcp_stdio_with_caller(caller: Caller) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = BufWriter::new(tokio::io::stdout());

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle_mcp_message(&caller, message).await,
            Err(err) => Some(error_response(
                Value::Null,
                -32700,
                format!("parse error: {err}"),
            )),
        };

        if let Some(response) = response {
            stdout
                .write_all(serde_json::to_string(&response)?.as_bytes())
                .await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

pub async fn handle_mcp_message(caller: &Caller, message: Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");

    if id.is_none() {
        return None;
    }

    let id = id.unwrap_or(Value::Null);
    match method {
        "initialize" => Some(success_response(id, initialize_result())),
        "ping" => Some(success_response(id, json!({}))),
        "tools/list" => Some(success_response(id, json!({ "tools": tools() }))),
        "tools/call" => {
            let result = call_tool(caller, &id, message).await;
            Some(success_response(id, result))
        }
        _ => Some(error_response(
            id,
            -32601,
            format!("method not found: {method}"),
        )),
    }
}

async fn call_tool(caller: &Caller, id: &Value, message: Value) -> Value {
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return tool_error("tools/call params.name is required");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let request = request_from_tool_call(id.clone(), name, arguments);
    response_to_tool_result(caller.handle(request).await)
}

fn request_from_tool_call(id: Value, name: &str, arguments: Value) -> ExecutorRequest {
    let mut params = match arguments {
        Value::Object(map) => map,
        _ => Map::new(),
    };

    let executor = take_string(&mut params, "targetExecutor")
        .or_else(|| take_string(&mut params, "target_executor"));
    let tool_timeout_ms =
        take_u64(&mut params, "toolTimeoutMs").or_else(|| take_u64(&mut params, "tool_timeout_ms"));
    let directory = take_path(&mut params, "directory");

    ExecutorRequest {
        id,
        method: name.to_string(),
        params: Value::Object(params),
        directory,
        executor,
        tool_timeout_ms,
    }
}

fn take_string(params: &mut Map<String, Value>, key: &str) -> Option<String> {
    params
        .remove(key)
        .and_then(|value| value.as_str().map(str::to_string))
}

fn take_path(params: &mut Map<String, Value>, key: &str) -> Option<PathBuf> {
    take_string(params, key).map(PathBuf::from)
}

fn take_u64(params: &mut Map<String, Value>, key: &str) -> Option<u64> {
    params.remove(key).and_then(|value| value.as_u64())
}

fn response_to_tool_result(response: ExecutorResponse) -> Value {
    if !response.ok {
        return tool_error(
            response
                .error
                .unwrap_or_else(|| "tool call failed".to_string()),
        );
    }

    let result = response.result.unwrap_or(Value::Null);
    let text = result
        .get("output")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
        });

    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": result,
        "isError": false,
    })
}

fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true,
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "remote-caller-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            "read",
            "Read a file or directory",
            schema(
                &["filePath"],
                &[
                    prop("filePath", "string"),
                    prop("offset", "number"),
                    prop("limit", "number"),
                ],
            ),
        ),
        tool(
            "glob",
            "Find files by glob pattern",
            schema(
                &["pattern"],
                &[prop("pattern", "string"), prop("path", "string")],
            ),
        ),
        tool(
            "grep",
            "Search file contents",
            schema(
                &["pattern"],
                &[
                    prop("pattern", "string"),
                    prop("path", "string"),
                    prop("include", "string"),
                ],
            ),
        ),
        tool(
            "apply_patch",
            "Apply opencode patch envelope",
            schema(&["patchText"], &[prop("patchText", "string")]),
        ),
        tool(
            "diffy",
            "Apply standard unified/git diff",
            schema(
                &["patchText"],
                &[prop("patchText", "string"), prop("strip", "number")],
            ),
        ),
        tool(
            "exbash",
            "Run a shell command; detaches if it exceeds async_timeout",
            schema(
                &["command"],
                &[
                    prop("command", "string"),
                    prop("description", "string"),
                    prop("workdir", "string"),
                    prop("timeout", "number"),
                    prop("async_timeout", "number"),
                ],
            ),
        ),
        tool(
            "exbash_list",
            "List exbash runs",
            schema(&[], &[prop("asyncID", "string")]),
        ),
        tool(
            "exbash_attach",
            "Write input and return a PTY snapshot after timeout",
            schema(
                &["asyncID"],
                &[
                    prop("asyncID", "string"),
                    prop("text", "string"),
                    prop("filePath", "string"),
                    prop("timeout", "number"),
                ],
            ),
        ),
        tool(
            "exbash_stop",
            "Stop an exbash run",
            schema(&["asyncID"], &[prop("asyncID", "string")]),
        ),
        tool(
            "exbash_remove",
            "Remove a stopped exbash run",
            schema(&["asyncID"], &[prop("asyncID", "string")]),
        ),
        tool(
            "rg",
            "Ripgrep-style search",
            schema(
                &["pattern"],
                &[
                    prop("pattern", "string"),
                    prop("root", "string"),
                    prop("path", "string"),
                    prop("globs", "array"),
                    prop("case_sensitive", "boolean"),
                    prop("max_count", "number"),
                ],
            ),
        ),
        tool(
            "list_executor",
            "List connected executors",
            schema(&[], &[]),
        ),
        tool(
            "connect_to_executor",
            "Connect a WebSocket Executor",
            schema(
                &["id", "url"],
                &[
                    prop("id", "string"),
                    prop("url", "string"),
                    prop("system", "string"),
                    prop("device", "string"),
                    prop("labels", "object"),
                ],
            ),
        ),
        tool(
            "set_default_executor",
            "Set the default Executor",
            schema(&["id"], &[prop("id", "string")]),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": add_routing(input_schema),
    })
}

fn schema(required: &[&str], props: &[Value]) -> Value {
    let mut properties = Map::new();
    for prop in props {
        if let Some(name) = prop.get("name").and_then(Value::as_str) {
            let mut prop = prop.clone();
            prop.as_object_mut().unwrap().remove("name");
            properties.insert(name.to_string(), prop);
        }
    }
    json!({ "type": "object", "properties": properties, "required": required })
}

fn prop(name: &str, kind: &str) -> Value {
    json!({ "name": name, "type": kind })
}

fn add_routing(mut schema: Value) -> Value {
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return schema;
    };
    properties.insert("targetExecutor".to_string(), json!({ "type": "string" }));
    properties.insert("directory".to_string(), json!({ "type": "string" }));
    schema
}
