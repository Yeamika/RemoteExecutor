use crate::exec_support::{
    attach, clip, description, input_data, list_run_details, manager, merge_json, remove_run,
    run_detail, start_job, stop_run, wait_for_stop_with_output,
};
use crate::{ToolContext, ToolResult};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

const READ_TIMEOUT: u64 = 10_000;
const INPUT_TIMEOUT: u64 = 10_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExbashOptions {
    #[serde(skip)]
    pub mode: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub timeout: Option<i64>,
    #[serde(default, rename = "read_timeout")]
    pub read_timeout: Option<u64>,
    #[serde(default, rename = "asyncID")]
    pub async_id: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, rename = "filePath")]
    pub file_path: Option<PathBuf>,
}

impl ExbashOptions {
    pub(crate) fn timeout_ms(&self) -> Result<Option<u64>> {
        match self.timeout {
            None | Some(-1) => Ok(None),
            Some(timeout) if timeout < -1 => Err(anyhow!("timeout must be -1 or non-negative")),
            Some(timeout) => Ok(Some(timeout as u64)),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ExbashOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub async fn exbash(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    match options.mode.as_deref() {
        None => run_command(options, ctx).await,
        Some("list") => list(options, ctx).await,
        Some("attach") => attach_input(options, ctx).await,
        Some("exbash_stop") => stop(options, ctx).await,
        Some("exbash_remove") => remove(options, ctx).await,
        Some(mode) => Err(anyhow!("unknown exbash mode: {mode}")),
    }
}

async fn run_command(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let read_timeout = options.read_timeout.unwrap_or(READ_TIMEOUT);
    let description = description(&options);
    let mut job = start_job(&options, ctx).await?;
    if let Some((detail, output)) = wait_for_stop_with_output(&mut job, read_timeout).await? {
        job.manager.remove_pty(&job.async_id);
        return Ok(ToolResult {
            title: description.clone(),
            metadata: json!({ "output": clip(&output), "exit": detail.exit_code, "description": description }),
            output,
        });
    }

    let detail = run_detail(
        &job.manager,
        &job.async_id,
        Some(job.description.clone()),
        job.timeout,
    )?;
    let mut value = serde_json::to_value(&detail)?;
    value["detached"] = json!(true);
    value["read_timeout"] = json!(read_timeout);
    Ok(ToolResult {
        title: description,
        metadata: value.clone(),
        output: serde_json::to_string_pretty(&value)?,
    })
}

async fn list(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let manager = manager(ctx)?;
    let runs = list_run_details(&manager, options.async_id.as_deref())?;
    let value = json!({ "runs": runs });
    Ok(ToolResult {
        title: "Async runs listed".to_string(),
        metadata: value.clone(),
        output: serde_json::to_string_pretty(&value)?,
    })
}

async fn stop(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let id = options
        .async_id
        .clone()
        .ok_or_else(|| anyhow!("asyncID is required"))?;
    let manager = manager(ctx)?;
    let detail = stop_run(&manager, &id).await?;
    Ok(ToolResult {
        title: "Async run stopped".to_string(),
        metadata: serde_json::to_value(&detail)?,
        output: serde_json::to_string_pretty(&detail)?,
    })
}

async fn remove(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let id = options
        .async_id
        .clone()
        .ok_or_else(|| anyhow!("asyncID is required"))?;
    let manager = manager(ctx)?;
    let value = remove_run(&manager, &id)?;
    Ok(ToolResult {
        title: "Async run removed".to_string(),
        metadata: value.clone(),
        output: serde_json::to_string_pretty(&value)?,
    })
}

async fn attach_input(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    if options.timeout.is_some() {
        return Err(anyhow!(
            "read_timeout is required instead of timeout for exbash_attach"
        ));
    }

    let id = options
        .async_id
        .clone()
        .ok_or_else(|| anyhow!("asyncID is required"))?;
    let manager = manager(ctx)?;
    let detail = manager.core().detail(&id)?;
    if detail.exit_code.is_some() {
        return Err(anyhow!("Async run {id} is not running"));
    }

    let (data, source) = input_data(&options, ctx).await?;
    let output_offset = detail.output_history_bytes;
    if !data.is_empty() {
        manager.core().send_to_pty(&id, &data)?;
    }

    let mut value = json!({
        "asyncID": id,
        "read_timeout": options.read_timeout.unwrap_or(INPUT_TIMEOUT),
        "wrote": data.len(),
        "source": source,
    });
    let (snapshot, attach_meta) = attach(
        &manager,
        &id,
        output_offset,
        options.read_timeout.unwrap_or(INPUT_TIMEOUT),
    )
    .await?;
    merge_json(&mut value, attach_meta);
    Ok(ToolResult {
        title: "Async input sent".to_string(),
        metadata: value.clone(),
        output: snapshot,
    })
}
