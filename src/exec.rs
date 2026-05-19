use crate::exec_support::{
    attach, clip, description, input_data, jobs, merge_json, refresh_job, start_job, stop_job,
    wait_for_stop,
};
use crate::{ToolContext, ToolResult};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

const ASYNC_TIMEOUT: u64 = 10_000;
const INPUT_TIMEOUT: u64 = 10_000;
const INPUT_WINDOW: usize = 100;

#[derive(Clone, Debug, Deserialize)]
pub struct ExbashOptions {
    pub mode: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    #[serde(default)]
    pub executor: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default, rename = "async_timeout")]
    pub async_timeout: Option<u64>,
    #[serde(default, rename = "asyncID")]
    pub async_id: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub wait: Option<String>,
    #[serde(default)]
    pub window: Option<usize>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, rename = "filePath")]
    pub file_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExbashOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub async fn exbash(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    match options.mode.as_str() {
        "exec_timeout_async" => exec_timeout_async(options, ctx).await,
        "exec_async" => exec_async(options, ctx).await,
        "list" => list(options).await,
        "control" => control(options).await,
        "input" => input(options, ctx).await,
        _ => Err(anyhow!("unknown exbash mode: {}", options.mode)),
    }
}

async fn exec_timeout_async(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let async_timeout = options.async_timeout.unwrap_or(ASYNC_TIMEOUT);
    let description = description(&options);
    let job = start_job(&options, ctx).await?;
    if wait_for_stop(&job, async_timeout).await {
        let detail = job.detail.lock().await.clone();
        let output = tokio::fs::read_to_string(&detail.result_path)
            .await
            .unwrap_or_default();
        jobs().lock().await.remove(&detail.async_id);
        return Ok(ToolResult {
            title: description.clone(),
            metadata: json!({ "output": clip(&output), "exit": detail.exit_code, "description": description }),
            output,
        });
    }

    let detail = job.detail.lock().await.clone();
    let mut value = serde_json::to_value(&detail)?;
    value["detached"] = json!(true);
    value["asyncTimeout"] = json!(async_timeout);
    Ok(ToolResult {
        title: description,
        metadata: value.clone(),
        output: serde_json::to_string_pretty(&value)?,
    })
}

async fn exec_async(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let job = start_job(&options, ctx).await?;
    let detail = job.detail.lock().await.clone();
    Ok(ToolResult {
        title: detail.description.clone(),
        metadata: serde_json::to_value(&detail)?,
        output: serde_json::to_string_pretty(&detail)?,
    })
}

async fn list(options: ExbashOptions) -> Result<ToolResult> {
    let jobs_snapshot = jobs().lock().await.values().cloned().collect::<Vec<_>>();
    let mut runs = Vec::new();
    for job in jobs_snapshot {
        refresh_job(&job).await?;
        let detail = job.detail.lock().await.clone();
        if options
            .async_id
            .as_deref()
            .is_some_and(|id| id != detail.async_id)
        {
            continue;
        }
        if options
            .scope
            .as_deref()
            .is_some_and(|scope| scope != detail.scope)
        {
            continue;
        }
        runs.push(detail);
    }
    let value = json!({ "runs": runs });
    Ok(ToolResult {
        title: "Async runs listed".to_string(),
        metadata: value.clone(),
        output: serde_json::to_string_pretty(&value)?,
    })
}

async fn control(options: ExbashOptions) -> Result<ToolResult> {
    let id = options
        .async_id
        .clone()
        .ok_or_else(|| anyhow!("asyncID is required"))?;
    let action = options
        .action
        .ok_or_else(|| anyhow!("action is required"))?;
    let job = jobs()
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("Async run not found: {id}"))?;

    match action.as_str() {
        "stop" => {
            stop_job(&job, 130, None).await?;
            let detail = job.detail.lock().await.clone();
            Ok(ToolResult {
                title: "Async run stopped".to_string(),
                metadata: serde_json::to_value(&detail)?,
                output: serde_json::to_string_pretty(&detail)?,
            })
        }
        "remove" => {
            refresh_job(&job).await?;
            if job.detail.lock().await.state != "stopped" {
                return Err(anyhow!("Async run {id} must be stopped before removal"));
            }
            let detail = job.detail.lock().await.clone();
            jobs().lock().await.remove(&id);
            let _ = tokio::fs::remove_file(&detail.result_path).await;
            let value = json!({ "asyncID": id, "removed": true, "resultPath": detail.result_path });
            Ok(ToolResult {
                title: "Async run removed".to_string(),
                metadata: value.clone(),
                output: serde_json::to_string_pretty(&value)?,
            })
        }
        _ => Err(anyhow!("unknown control action: {action}")),
    }
}

async fn input(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let id = options
        .async_id
        .clone()
        .ok_or_else(|| anyhow!("asyncID is required"))?;
    let job = jobs()
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("Async run not found: {id}"))?;
    refresh_job(&job).await?;
    if job.detail.lock().await.state != "running" {
        return Err(anyhow!("Async run {id} is not running"));
    }

    let detail = job.detail.lock().await.clone();
    let offset = tokio::fs::metadata(&detail.result_path)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0);
    let (data, source) = input_data(&options, ctx).await?;
    if !data.is_empty() {
        let mut child = job.child.lock().await;
        let child = child
            .as_mut()
            .ok_or_else(|| anyhow!("Async run {id} is not running"))?;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("Async run {id} is not accepting stdin"))?;
        stdin.write_all(&data).await?;
    }

    let wait = options.wait.clone().unwrap_or_else(|| "return".to_string());
    let mut value = json!({
        "asyncID": id,
        "wait": wait,
        "wrote": data.len(),
        "source": source,
    });
    if value["wait"] == "attach" {
        let tail = attach(
            &detail.result_path,
            offset,
            options.timeout.unwrap_or(INPUT_TIMEOUT),
            options.window.unwrap_or(INPUT_WINDOW),
        )
        .await;
        merge_json(&mut value, tail);
    }

    Ok(ToolResult {
        title: "Async input sent".to_string(),
        metadata: value.clone(),
        output: serde_json::to_string_pretty(&value)?,
    })
}
