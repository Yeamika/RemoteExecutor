use crate::exec_support::{
    attach, captured_output, captured_output_len, clip, description, input_data, jobs, merge_json,
    refresh_job, remove_job_pty, start_job, stop_job, wait_for_stop,
};
use crate::{ToolContext, ToolResult};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

const ASYNC_TIMEOUT: u64 = 10_000;
const INPUT_TIMEOUT: u64 = 10_000;

#[derive(Clone, Debug, Deserialize)]
pub struct ExbashOptions {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default, rename = "async_timeout")]
    pub async_timeout: Option<u64>,
    #[serde(default, rename = "asyncID")]
    pub async_id: Option<String>,
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
    let mode = options.mode.as_deref().unwrap_or("exec_timeout_async");
    match mode {
        "exec_timeout_async" => exec_timeout_async(options, ctx).await,
        "exec_async" => exec_async(options, ctx).await,
        "list" => list(options).await,
        "attach" => attach_input(options, ctx).await,
        "exbash_stop" | "stop" => stop(options).await,
        "exbash_remove" | "exbasp_remove" | "remove" => remove(options).await,
        _ => Err(anyhow!("unknown exbash mode: {mode}")),
    }
}

async fn exec_timeout_async(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let async_timeout = options.async_timeout.unwrap_or(ASYNC_TIMEOUT);
    let description = description(&options);
    let job = start_job(&options, ctx).await?;
    if wait_for_stop(&job, async_timeout).await {
        let detail = job.detail.lock().await.clone();
        let output = captured_output(&job).await;
        jobs().lock().await.remove(&detail.async_id);
        remove_job_pty(&job);
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
        runs.push(detail);
    }
    let value = json!({ "runs": runs });
    Ok(ToolResult {
        title: "Async runs listed".to_string(),
        metadata: value.clone(),
        output: serde_json::to_string_pretty(&value)?,
    })
}

async fn stop(options: ExbashOptions) -> Result<ToolResult> {
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

    stop_job(&job, 130, None).await?;
    let detail = job.detail.lock().await.clone();
    Ok(ToolResult {
        title: "Async run stopped".to_string(),
        metadata: serde_json::to_value(&detail)?,
        output: serde_json::to_string_pretty(&detail)?,
    })
}

async fn remove(options: ExbashOptions) -> Result<ToolResult> {
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
    if job.detail.lock().await.state != "stopped" {
        return Err(anyhow!("Async run {id} must be stopped before removal"));
    }
    jobs().lock().await.remove(&id);
    remove_job_pty(&job);
    let value = json!({ "asyncID": id, "removed": true });
    Ok(ToolResult {
        title: "Async run removed".to_string(),
        metadata: value.clone(),
        output: serde_json::to_string_pretty(&value)?,
    })
}

async fn attach_input(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
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
    let (data, source) = input_data(&options, ctx).await?;
    let output_offset = captured_output_len(&job).await;
    if !data.is_empty() {
        job.manager.core().send_to_pty(&detail.async_id, &data)?;
    }

    let mut value = json!({
        "asyncID": id,
        "wrote": data.len(),
        "source": source,
    });
    let (snapshot, attach_meta) = attach(
        &job,
        output_offset,
        options.timeout.unwrap_or(INPUT_TIMEOUT),
    )
    .await?;
    merge_json(&mut value, attach_meta);
    Ok(ToolResult {
        title: "Async input sent".to_string(),
        metadata: value.clone(),
        output: snapshot,
    })
}
