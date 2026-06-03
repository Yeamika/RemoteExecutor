use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

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
    #[serde(rename = "tool")]
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    #[serde(
        default,
        rename = "toolTimeoutMs",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_timeout_ms: Option<u64>,
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

#[derive(Clone, Debug, Serialize)]
pub struct ToolResult {
    pub metadata: Value,
    pub output: Value,
}

pub fn tool_output(text: impl Into<String>) -> Value {
    json!({ "message": "", "text": text.into(), "info": "" })
}

pub fn tool_output_with_info(text: impl Into<String>, info: impl Into<String>) -> Value {
    json!({ "message": "", "text": text.into(), "info": info.into() })
}

pub fn tool_output_full(
    message: impl Into<String>,
    text: impl Into<String>,
    info: impl Into<String>,
) -> Value {
    json!({ "message": message.into(), "text": text.into(), "info": info.into() })
}
