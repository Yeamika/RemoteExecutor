use crate::exec::ExbashOptions;
use crate::{ShellManager, ToolContext};
use anyhow::{anyhow, Result};
use pty_t_core::{CommandSpec, SessionDetail};
use serde::Serialize;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
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
    pub(crate) exit_code: Option<i32>,
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
    let session = manager.create_pty(id.clone(), command_spec(&command, &cwd)?, None, None)?;
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
                .map(|code| format!(" exit={code}"))
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
    session.kill()?;
    let _ = manager
        .core()
        .wait_exit_code_timeout(async_id, Duration::from_millis(500))
        .await?;
    run_detail(manager, async_id, None, None)
}

pub(crate) fn remove_run(manager: &ShellManager, async_id: &str) -> Result<serde_json::Value> {
    let detail = manager.core().detail(async_id)?;
    if detail.exit_code.is_none() {
        return Err(anyhow!(
            "Async run {async_id} must be stopped before removal"
        ));
    }
    manager.remove_pty(async_id);
    Ok(json!({ "asyncID": async_id, "removed": true }))
}

pub(crate) async fn attach(
    manager: &ShellManager,
    async_id: &str,
    output_offset: usize,
    timeout: u64,
) -> Result<(String, serde_json::Value)> {
    time::sleep(Duration::from_millis(timeout)).await;
    let text = manager.core().snapshot_pty_plain(async_id)?;
    let output_bytes = manager
        .core()
        .detail(async_id)?
        .output_history_bytes
        .saturating_sub(output_offset);
    Ok((text, json!({ "outputBytes": output_bytes })))
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
        return Ok((text.into_bytes(), "text"));
    }
    if let Some(path) = options.file_path_input() {
        return Ok((tokio::fs::read(ctx.resolve(path)).await?, "file"));
    }
    Ok((Vec::new(), "attach"))
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
    let exit_code = detail.exit_code.map(|code| code as i32);
    let state = if exit_code.is_some() {
        "stopped"
    } else {
        "running"
    }
    .to_string();
    let status = exit_code
        .map(|code| format!("stopped (exit {code})"))
        .unwrap_or_else(|| "running".to_string());
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
        ended_at: exit_code.map(|_| now_ms()),
        error: None,
    }
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
                let _ = session.kill();
            }
        }
    });
}

fn command_spec(command: &str, cwd: &std::path::Path) -> Result<CommandSpec> {
    let parts = shell_words::split(command)
        .map_err(|err| anyhow!("failed to parse command arguments: {err}"))?;
    let Some((program, args)) = parts.split_first() else {
        return Err(anyhow!("command is required"));
    };
    Ok(CommandSpec::new(program.clone())
        .args(args.iter().map(String::as_str))
        .cwd(cwd.to_path_buf()))
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
