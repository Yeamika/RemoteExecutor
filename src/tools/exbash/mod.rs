mod attach;
mod input;
mod options;
mod run;
mod runs;

#[cfg(test)]
mod test;

use crate::{tool_output, tool_output_full, ToolContext, ToolResult};
use anyhow::{anyhow, Result};
use runs::{
    attach, clear_exit_code_label, clip, exit_code_display, exit_code_json, format_run_details,
    input_data, list_run_details, manager, merge_json, remove_run, run_detail, start_job, stop_run,
    wait_for_stop_with_output,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

const READ_TIMEOUT: u64 = 10_000;
const INPUT_TIMEOUT: u64 = 10_000;
const INPUT_BYTES_LIMIT: usize = 4096;
const DESCRIPTION_BYTES_LIMIT: usize = 100;
const ASYNC_ID_BYTES_LIMIT: usize = 30;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExbashOptions {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(skip)]
    pub shell: bool,
    #[serde(default, rename = "shell")]
    pub shell_profile: Option<String>,
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
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    #[serde(default, rename = "showRawPretty")]
    pub show_raw_pretty: bool,
}

fn validate_optional_bytes(name: &str, value: Option<&str>, limit: usize) -> Result<()> {
    if let Some(value) = value {
        validate_bytes(name, value, limit)?;
    }
    Ok(())
}

fn validate_bytes(name: &str, value: &str, limit: usize) -> Result<()> {
    let len = value.len();
    if len > limit {
        return Err(anyhow!("{name} exceeds {limit} bytes ({len} bytes)"));
    }
    Ok(())
}

impl ExbashOptions {
    pub(crate) const INPUT_BYTES_LIMIT: usize = INPUT_BYTES_LIMIT;

    pub(crate) fn validate_input_limits(&self) -> Result<()> {
        validate_optional_bytes("command", self.command.as_deref(), INPUT_BYTES_LIMIT)?;
        validate_optional_bytes(
            "description",
            self.description.as_deref(),
            DESCRIPTION_BYTES_LIMIT,
        )?;
        validate_optional_bytes("asyncID", self.async_id.as_deref(), ASYNC_ID_BYTES_LIMIT)?;
        validate_optional_bytes("text", self.text.as_deref(), INPUT_BYTES_LIMIT)?;
        validate_optional_bytes("shell", self.shell_profile.as_deref(), INPUT_BYTES_LIMIT)?;
        if let Some(path) = self.file_path_input() {
            let value = path.to_string_lossy();
            validate_bytes("filePath", &value, INPUT_BYTES_LIMIT)?;
        }
        if let Some(path) = self
            .workdir
            .as_ref()
            .filter(|path| !path.as_os_str().is_empty())
        {
            let value = path.to_string_lossy();
            validate_bytes("workdir", &value, INPUT_BYTES_LIMIT)?;
        }
        Ok(())
    }

    pub(crate) fn timeout_ms(&self) -> Result<Option<u64>> {
        match self.timeout {
            None | Some(-1 | 0) => Ok(None),
            Some(timeout) if timeout < -1 => Err(anyhow!("timeout must be -1 or non-negative")),
            Some(timeout) => Ok(Some(timeout as u64)),
        }
    }

    pub(crate) fn text_input(&self) -> Option<&str> {
        self.text.as_deref().filter(|text| !text.is_empty())
    }

    pub(crate) fn file_path_input(&self) -> Option<&PathBuf> {
        self.file_path
            .as_ref()
            .filter(|path| !path.as_os_str().is_empty())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ExbashOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub async fn exbash(mut options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    options.validate_input_limits()?;
    match options.mode.as_deref().unwrap_or_default() {
        "run" => run_command(options, ctx).await,
        "shell" => {
            options.shell = true;
            run_command(options, ctx).await
        }
        "attach" => attach_input(options, ctx).await,
        "list" => list(options, ctx).await,
        "stop" => stop(options, ctx).await,
        "remove" => remove(options, ctx).await,
        "" => Err(anyhow!("exbash mode is required")),
        mode => Err(anyhow!("unknown exbash mode: {mode}")),
    }
}

async fn run_command(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let read_timeout = options.read_timeout.unwrap_or(READ_TIMEOUT);
    let mut job = start_job(&options, ctx).await?;
    if let Some((detail, output)) = wait_for_stop_with_output(&mut job, read_timeout).await? {
        job.manager.remove_pty(&job.async_id);
        clear_exit_code_label(&job.async_id);
        return Ok(ToolResult {
            metadata: json!({ "output": clip(&output), "exitCode": detail.exit_code }),
            output: tool_output(output),
        });
    }

    let detail = run_detail(
        &job.manager,
        &job.async_id,
        Some(job.description.clone()),
        job.timeout,
    )?;
    let snapshot = job.manager.core().snapshot_pty_plain(&job.async_id)?;
    let mut value = serde_json::to_value(&detail)?;
    value["detached"] = json!(true);
    let message = format!("{} detached", job.async_id);
    Ok(ToolResult {
        metadata: value.clone(),
        output: tool_output_full(message, snapshot, ""),
    })
}

async fn list(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let manager = manager(ctx)?;
    let runs = list_run_details(&manager, options.async_id.as_deref())?;
    let value = json!({ "runs": runs });
    Ok(ToolResult {
        metadata: value.clone(),
        output: tool_output(format_run_details(&runs)),
    })
}

async fn stop(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let id = options
        .async_id
        .clone()
        .ok_or_else(|| anyhow!("asyncID is required"))?;
    let manager = manager(ctx)?;
    let detail = stop_run(&manager, &id).await?;
    let output = manager.core().snapshot_pty_plain(&id)?;
    Ok(ToolResult {
        metadata: serde_json::to_value(&detail)?,
        output: tool_output(output),
    })
}

async fn remove(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let id = options
        .async_id
        .clone()
        .ok_or_else(|| anyhow!("asyncID is required"))?;
    let manager = manager(ctx)?;
    remove_run(&manager, &id).await?;
    Ok(ToolResult {
        metadata: json!({ "ok": true }),
        output: tool_output("ok"),
    })
}

async fn attach_input(options: ExbashOptions, ctx: &ToolContext) -> Result<ToolResult> {
    if options.timeout.is_some() {
        return Err(anyhow!(
            "read_timeout is required instead of timeout for mode attach"
        ));
    }

    let id = options
        .async_id
        .clone()
        .ok_or_else(|| anyhow!("asyncID is required"))?;
    let manager = manager(ctx)?;
    let detail = manager.core().detail(&id)?;
    if let Some(exit_code) = detail.exit_code {
        let input_failed = options.text_input().is_some() || options.file_path_input().is_some();
        let exit_code_display = exit_code_display(&id, exit_code);
        let message = stopped_attach_message(&exit_code_display, input_failed);
        let exit_code_value =
            exit_code_json(&id, Some(exit_code)).unwrap_or_else(|| json!(exit_code));
        let mut value = json!({
            "asyncID": id,
            "wrote": 0,
            "source": requested_input_source(&options),
            "outputBytes": detail.output_history_bytes,
            "state": "stopped",
            "exitCode": exit_code_value,
        });
        add_raw_pretty(&manager, &id, &mut value, options.show_raw_pretty)?;
        return Ok(ToolResult {
            metadata: value,
            output: tool_output_full(message, manager.core().snapshot_pty_plain(&id)?, ""),
        });
    }

    let (data, source) = input_data(&options, ctx).await?;
    let output_offset = detail.output_history_bytes;
    let controller = (!data.is_empty()).then(|| format!("rec:{id}"));
    if !data.is_empty() {
        let session = manager
            .core()
            .session(&id)
            .ok_or_else(|| anyhow!("Async run not found: {id}"))?;
        let controller = controller.as_ref().unwrap();
        session.force_controller(controller);
        manager.broadcast_meta(&id);
        if !session.write_from_controller(controller, &data)? {
            let attached_by = session
                .controller_id()
                .unwrap_or_else(|| "unknown".to_string());
            return Err(anyhow!("control lost: someone attached: {attached_by}"));
        }
    }

    let mut value = json!({
        "asyncID": id,
        "wrote": data.len(),
        "source": source,
    });
    let (snapshot, attach_meta) = attach(
        &manager,
        &id,
        output_offset,
        options.read_timeout.unwrap_or(INPUT_TIMEOUT),
        controller.as_deref(),
    )
    .await?;
    merge_json(&mut value, attach_meta);
    let message = take_message(&mut value);
    add_raw_pretty(&manager, &id, &mut value, options.show_raw_pretty)?;
    Ok(ToolResult {
        metadata: value.clone(),
        output: tool_output_full(message, snapshot, ""),
    })
}

fn take_message(value: &mut serde_json::Value) -> String {
    let Some(object) = value.as_object_mut() else {
        return String::new();
    };
    object
        .remove("message")
        .and_then(|message| message.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn add_raw_pretty(
    manager: &crate::ShellManager,
    id: &str,
    value: &mut serde_json::Value,
    show: bool,
) -> Result<()> {
    if !show {
        return Ok(());
    }
    let formatted = manager.core().snapshot_pty(id)?;
    merge_json(
        value,
        json!({
            "rawPretty": String::from_utf8_lossy(&formatted),
        }),
    );
    Ok(())
}

fn requested_input_source(options: &ExbashOptions) -> &'static str {
    match (
        options.text_input().is_some(),
        options.file_path_input().is_some(),
    ) {
        (true, false) => "text",
        (false, true) => "file",
        (true, true) => "input",
        (false, false) => "attach",
    }
}

fn stopped_attach_message(exit_code: &str, input_failed: bool) -> String {
    let message = format!("task already exited with code {exit_code}");
    if input_failed {
        format!("input failed: {message}")
    } else {
        message
    }
}
