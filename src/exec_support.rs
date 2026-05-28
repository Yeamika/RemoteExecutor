use crate::exec::ExbashOptions;
use crate::{ShellManager, ToolContext};
use anyhow::{anyhow, Result};
use pty_t_core::{CommandSpec, SessionDetail};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time;

const OUTPUT_LIMIT: usize = 30_000;
const EXBASH_PREFIX: &str = "rex-";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RunDetail {
    #[serde(rename = "asyncID")]
    pub(crate) async_id: String,
    pub(crate) pid: Option<u32>,
    pub(crate) status: String,
    pub(crate) state: String,
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<Value>,
    #[serde(rename = "totalOutput")]
    pub(crate) total_output: usize,
    pub(crate) command: String,
    pub(crate) description: String,
    pub(crate) cwd: String,
    pub(crate) timeout: Option<i64>,
    #[serde(rename = "startedAt")]
    pub(crate) started_at: u128,
    #[serde(rename = "endedAt", skip_serializing_if = "Option::is_none")]
    pub(crate) ended_at: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

pub(crate) struct StartedJob {
    pub(crate) manager: ShellManager,
    pub(crate) async_id: String,
    pub(crate) description: String,
    pub(crate) timeout: Option<i64>,
    output: mpsc::UnboundedReceiver<Vec<u8>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static EXIT_CODE_LABELS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn manager(ctx: &ToolContext) -> Result<ShellManager> {
    ctx.shell_manager()
        .ok_or_else(|| anyhow!("exbash requires a PTY-backed ShellManager"))
}

pub(crate) async fn start_job(options: &ExbashOptions, ctx: &ToolContext) -> Result<StartedJob> {
    let command = options
        .command
        .clone()
        .ok_or_else(|| anyhow!("command is required"))?;
    let manager = manager(ctx)?;
    let cwd = ctx.directory.clone();
    let id = next_id();
    let timeout_ms = options.timeout_ms()?;
    let session = manager.create_pty(
        id.clone(),
        command_spec(&command, &cwd, options.shell)?,
        None,
        None,
    )?;
    let output = session.subscribe_output();

    if let Some(timeout) = timeout_ms {
        spawn_timeout(manager.clone(), id.clone(), timeout);
    }

    Ok(StartedJob {
        manager,
        async_id: id,
        description: description(options),
        timeout: options.timeout,
        output,
    })
}

pub(crate) async fn wait_for_stop_with_output(
    job: &mut StartedJob,
    timeout: u64,
) -> Result<Option<(RunDetail, String)>> {
    let deadline = time::Instant::now() + Duration::from_millis(timeout);
    let mut output = Vec::new();

    loop {
        drain_output(&mut job.output, &mut output);
        if job.manager.core().try_exit_code(&job.async_id)?.is_some() {
            time::sleep(Duration::from_millis(20)).await;
            drain_output(&mut job.output, &mut output);
            let detail = run_detail(
                &job.manager,
                &job.async_id,
                Some(job.description.clone()),
                job.timeout,
            )?;
            return Ok(Some((
                detail,
                String::from_utf8_lossy(&output).into_owned(),
            )));
        }

        let now = time::Instant::now();
        if now >= deadline {
            drain_output(&mut job.output, &mut output);
            return Ok(None);
        }
        time::sleep((deadline - now).min(Duration::from_millis(20))).await;
    }
}

pub(crate) fn run_detail(
    manager: &ShellManager,
    async_id: &str,
    description_override: Option<String>,
    timeout: Option<i64>,
) -> Result<RunDetail> {
    let detail = manager.core().detail(async_id)?;
    Ok(run_detail_from_session(
        detail,
        description_override,
        timeout,
    ))
}

pub(crate) fn list_run_details(
    manager: &ShellManager,
    filter: Option<&str>,
) -> Result<Vec<RunDetail>> {
    let mut runs = Vec::new();
    for summary in manager.core().list() {
        if !summary.pty.starts_with(EXBASH_PREFIX) {
            continue;
        }
        if filter.is_some_and(|id| id != summary.pty) {
            continue;
        }
        runs.push(run_detail(manager, &summary.pty, None, None)?);
    }
    Ok(runs)
}

pub(crate) fn format_run_details(runs: &[RunDetail]) -> String {
    if runs.is_empty() {
        return "No async runs".to_string();
    }
    runs.iter()
        .map(|run| {
            let exit = run
                .exit_code
                .as_ref()
                .map(|code| format!(" exit={}", exit_code_value_text(code)))
                .unwrap_or_default();
            format!(
                "{} {}{} totalOutput={} command={}",
                run.async_id, run.state, exit, run.total_output, run.command
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) async fn stop_run(manager: &ShellManager, async_id: &str) -> Result<RunDetail> {
    let session = manager
        .core()
        .session(async_id)
        .ok_or_else(|| anyhow!("Async run not found: {async_id}"))?;
    set_exit_code_label(async_id, "stopped");
    session.kill()?;
    let _ = manager
        .core()
        .wait_exit_code_timeout(async_id, Duration::from_millis(500))
        .await?;
    run_detail(manager, async_id, None, None)
}

pub(crate) async fn remove_run(
    manager: &ShellManager,
    async_id: &str,
) -> Result<serde_json::Value> {
    let detail = manager.core().detail(async_id)?;
    let mut stopped = false;
    let exit_code = if let Some(code) = detail.exit_code {
        Some(code)
    } else {
        let session = manager
            .core()
            .session(async_id)
            .ok_or_else(|| anyhow!("Async run not found: {async_id}"))?;
        session.kill()?;
        stopped = true;
        manager
            .core()
            .wait_exit_code_timeout(async_id, Duration::from_millis(500))
            .await?
    };

    let mut value = json!({
        "asyncID": async_id,
        "removed": true,
        "stopped": stopped,
    });
    if let Some(code) = exit_code {
        value["exitCode"] = json!(code);
    }
    manager.remove_pty(async_id);
    clear_exit_code_label(async_id);
    Ok(value)
}

pub(crate) fn clear_exit_code_label(async_id: &str) {
    EXIT_CODE_LABELS.lock().unwrap().remove(async_id);
}

pub(crate) fn exit_code_display(async_id: &str, code: u32) -> String {
    exit_code_label(async_id).unwrap_or_else(|| code.to_string())
}

pub(crate) fn exit_code_json(async_id: &str, code: Option<u32>) -> Option<Value> {
    code.map(|code| {
        exit_code_label(async_id)
            .map(Value::String)
            .unwrap_or_else(|| json!(code))
    })
}

pub(crate) async fn attach(
    manager: &ShellManager,
    async_id: &str,
    output_offset: usize,
    timeout: u64,
    controller: Option<&str>,
) -> Result<(String, serde_json::Value)> {
    wait_attach_timeout(manager, async_id, timeout, controller).await?;
    let text = manager.core().snapshot_pty_plain(async_id)?;
    let output_bytes = manager
        .core()
        .detail(async_id)?
        .output_history_bytes
        .saturating_sub(output_offset);
    Ok((text, json!({ "outputBytes": output_bytes })))
}

async fn wait_attach_timeout(
    manager: &ShellManager,
    async_id: &str,
    timeout: u64,
    controller: Option<&str>,
) -> Result<()> {
    let deadline = time::Instant::now() + Duration::from_millis(timeout);
    loop {
        if let Some(controller) = controller {
            let session = manager
                .core()
                .session(async_id)
                .ok_or_else(|| anyhow!("Async run not found: {async_id}"))?;
            let current = session.controller_id();
            if current.as_deref() != Some(controller) {
                let attached_by = current.unwrap_or_else(|| "unknown".to_string());
                return Err(anyhow!("control lost: someone attached: {attached_by}"));
            }
        }

        let now = time::Instant::now();
        if now >= deadline {
            return Ok(());
        }
        time::sleep((deadline - now).min(Duration::from_millis(20))).await;
    }
}

pub(crate) async fn input_data(
    options: &ExbashOptions,
    ctx: &ToolContext,
) -> Result<(Vec<u8>, &'static str)> {
    if options.text_input().is_some() && options.file_path_input().is_some() {
        return Err(anyhow!("Provide only one of text or filePath for attach."));
    }
    if let Some(text) = options.text_input() {
        let text = unescaper::unescape(text)
            .map_err(|err| anyhow!("failed to parse attach text escapes: {err}"))?;
        let bytes = text.into_bytes();
        validate_input_bytes("text", bytes.len())?;
        return Ok((bytes, "text"));
    }
    if let Some(path) = options.file_path_input() {
        let bytes = tokio::fs::read(ctx.resolve(path)).await?;
        validate_input_bytes("file input", bytes.len())?;
        return Ok((bytes, "file"));
    }
    Ok((Vec::new(), "attach"))
}

fn validate_input_bytes(name: &str, len: usize) -> Result<()> {
    let limit = ExbashOptions::INPUT_BYTES_LIMIT;
    if len > limit {
        return Err(anyhow!("{name} exceeds {limit} bytes ({len} bytes)"));
    }
    Ok(())
}

pub(crate) fn description(options: &ExbashOptions) -> String {
    options.description.clone().unwrap_or_else(|| {
        options
            .command
            .clone()
            .or_else(|| options.mode.clone())
            .unwrap_or_else(|| "exbash".to_string())
    })
}

pub(crate) fn clip(text: &str) -> String {
    if text.len() <= OUTPUT_LIMIT {
        text.to_string()
    } else {
        format!("{}\n\n...", &text[..OUTPUT_LIMIT])
    }
}

pub(crate) fn merge_json(target: &mut serde_json::Value, source: serde_json::Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn run_detail_from_session(
    detail: SessionDetail,
    description_override: Option<String>,
    timeout: Option<i64>,
) -> RunDetail {
    let command = detail.command.join(" ");
    let raw_exit_code = detail.exit_code;
    let exit_code = exit_code_json(&detail.pty, raw_exit_code);
    let state = if exit_code.is_some() {
        "stopped"
    } else {
        "running"
    }
    .to_string();
    let status = exit_code
        .as_ref()
        .map(|code| format!("stopped (exit {})", exit_code_value_text(code)))
        .unwrap_or_else(|| "running".to_string());
    let ended_at = exit_code.as_ref().map(|_| now_ms());
    RunDetail {
        async_id: detail.pty,
        pid: detail.process_id,
        status,
        state,
        exit_code,
        total_output: detail.output_history_bytes,
        command: command.clone(),
        description: description_override.unwrap_or(command),
        cwd: detail.cwd.unwrap_or_default(),
        timeout,
        started_at: u128::from(detail.created_at),
        ended_at,
        error: None,
    }
}

fn exit_code_label(async_id: &str) -> Option<String> {
    EXIT_CODE_LABELS.lock().unwrap().get(async_id).cloned()
}

fn exit_code_value_text(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn set_exit_code_label(async_id: &str, label: &str) {
    EXIT_CODE_LABELS
        .lock()
        .unwrap()
        .insert(async_id.to_string(), label.to_string());
}

fn drain_output(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>, output: &mut Vec<u8>) {
    while let Ok(chunk) = rx.try_recv() {
        output.extend(chunk);
    }
}

fn spawn_timeout(manager: ShellManager, async_id: String, timeout: u64) {
    tokio::spawn(async move {
        time::sleep(Duration::from_millis(timeout)).await;
        if manager
            .core()
            .try_exit_code(&async_id)
            .ok()
            .flatten()
            .is_none()
        {
            if let Some(session) = manager.core().session(&async_id) {
                set_exit_code_label(&async_id, "timeout");
                let _ = session.kill();
            }
        }
    });
}

fn command_spec(command: &str, cwd: &std::path::Path, shell: bool) -> Result<CommandSpec> {
    if shell {
        return Ok(shell_command_spec(command, cwd));
    }

    let parts = shell_words::split(command)
        .map_err(|err| anyhow!("failed to parse command arguments: {err}"))?;
    let Some((program, args)) = parts.split_first() else {
        return Err(anyhow!("command is required"));
    };
    Ok(CommandSpec::new(program.clone())
        .args(args.iter().map(String::as_str))
        .cwd(cwd.to_path_buf()))
}

fn shell_command_spec(command: &str, cwd: &std::path::Path) -> CommandSpec {
    if cfg!(windows) {
        CommandSpec::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                command,
            ])
            .cwd(cwd.to_path_buf())
    } else {
        CommandSpec::new("sh")
            .args(["-c", command])
            .cwd(cwd.to_path_buf())
    }
}

fn next_id() -> String {
    format!(
        "{}{}-{}",
        EXBASH_PREFIX,
        now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
