use crate::exec::ExbashOptions;
use crate::{ShellManager, ToolContext};
use anyhow::{anyhow, Result};
use pty_t_core::CommandSpec;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time;

const OUTPUT_LIMIT: usize = 30_000;
const CAPTURE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RunDetail {
    #[serde(rename = "asyncID")]
    pub(crate) async_id: String,
    pub(crate) pid: Option<u32>,
    pub(crate) status: String,
    pub(crate) state: String,
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    #[serde(rename = "linePointer")]
    pub(crate) line_pointer: usize,
    pub(crate) command: String,
    pub(crate) description: String,
    pub(crate) cwd: String,
    pub(crate) timeout: Option<u64>,
    #[serde(rename = "startedAt")]
    pub(crate) started_at: u128,
    #[serde(rename = "endedAt", skip_serializing_if = "Option::is_none")]
    pub(crate) ended_at: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

pub(crate) struct Job {
    pub(crate) detail: Mutex<RunDetail>,
    pub(crate) manager: ShellManager,
    pty: String,
    output: Mutex<Vec<u8>>,
}

static JOBS: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) async fn start_job(options: &ExbashOptions, ctx: &ToolContext) -> Result<Arc<Job>> {
    let command = options
        .command
        .clone()
        .ok_or_else(|| anyhow!("command is required"))?;
    let manager = ctx
        .shell_manager()
        .ok_or_else(|| anyhow!("exbash requires a PTY-backed ShellManager"))?;
    let cwd = cwd_for(options, ctx);
    let id = next_id();
    let spec = command_spec(&command, &cwd);
    let session = manager.create_pty(id.clone(), spec, None, None)?;
    let output = session.subscribe_output();
    let detail = RunDetail {
        async_id: id.clone(),
        pid: session.process_id(),
        status: "running".to_string(),
        state: "running".to_string(),
        exit_code: None,
        line_pointer: 0,
        command,
        description: description(options),
        cwd: cwd.to_string_lossy().into_owned(),
        timeout: options.timeout,
        started_at: now_ms(),
        ended_at: None,
        error: None,
    };
    let job = Arc::new(Job {
        detail: Mutex::new(detail),
        manager,
        pty: id.clone(),
        output: Mutex::new(Vec::new()),
    });
    jobs().lock().await.insert(id, job.clone());
    spawn_output_capture(output, job.clone());
    if let Some(timeout) = options.timeout {
        spawn_timeout(job.clone(), timeout);
    }
    Ok(job)
}

pub(crate) async fn refresh_job(job: &Arc<Job>) -> Result<()> {
    if job.detail.lock().await.state == "stopped" {
        return Ok(());
    }
    if let Some(code) = job.manager.core().try_exit_code(&job.pty)? {
        let mut detail = job.detail.lock().await;
        finish_detail(&mut detail, code as i32, None);
    }
    Ok(())
}

pub(crate) async fn stop_job(job: &Arc<Job>, code: i32, error: Option<String>) -> Result<()> {
    let _ = job.manager.core().kill_pty(&job.pty);
    let mut detail = job.detail.lock().await;
    finish_detail(&mut detail, code, error);
    Ok(())
}

pub(crate) fn remove_job_pty(job: &Arc<Job>) {
    job.manager.remove_pty(&job.pty);
}

pub(crate) async fn wait_for_stop(job: &Arc<Job>, timeout: u64) -> bool {
    let end = now_ms() + u128::from(timeout);
    loop {
        let _ = refresh_job(job).await;
        if job.detail.lock().await.state == "stopped" {
            return true;
        }
        if now_ms() >= end {
            return false;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
}

pub(crate) async fn input_data(
    options: &ExbashOptions,
    ctx: &ToolContext,
) -> Result<(Vec<u8>, &'static str)> {
    if options.text.is_some() && options.file_path.is_some() {
        return Err(anyhow!(
            "Provide only one of text or filePath for input mode."
        ));
    }
    if let Some(text) = &options.text {
        return Ok((text.as_bytes().to_vec(), "text"));
    }
    if let Some(path) = &options.file_path {
        return Ok((tokio::fs::read(ctx.resolve(path)).await?, "file"));
    }
    Ok((Vec::new(), "attach"))
}

pub(crate) async fn attach(
    job: &Arc<Job>,
    output_offset: usize,
    timeout: u64,
) -> Result<(String, serde_json::Value)> {
    time::sleep(Duration::from_millis(timeout)).await;
    refresh_job(job).await?;
    let snapshot = String::from_utf8_lossy(&job.manager.snapshot(&job.pty)?).into_owned();
    let output_bytes = captured_output_len(job).await.saturating_sub(output_offset);
    Ok((snapshot, json!({ "outputBytes": output_bytes })))
}

pub(crate) async fn captured_output(job: &Arc<Job>) -> String {
    String::from_utf8_lossy(&job.output.lock().await).into_owned()
}

pub(crate) async fn captured_output_len(job: &Arc<Job>) -> usize {
    job.output.lock().await.len()
}

pub(crate) fn jobs() -> &'static Mutex<HashMap<String, Arc<Job>>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
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

fn spawn_output_capture(mut output: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, job: Arc<Job>) {
    tokio::spawn(async move {
        while let Some(data) = output.recv().await {
            {
                let mut captured = job.output.lock().await;
                captured.extend(&data);
                if captured.len() > CAPTURE_LIMIT {
                    let remove = captured.len() - CAPTURE_LIMIT;
                    captured.drain(..remove);
                }
            }
            job.detail.lock().await.line_pointer += count_lines(&data);
        }
    });
}

fn spawn_timeout(job: Arc<Job>, timeout: u64) {
    tokio::spawn(async move {
        time::sleep(Duration::from_millis(timeout)).await;
        if job.detail.lock().await.state == "running" {
            let _ = stop_job(&job, 124, Some("timeout".to_string())).await;
        }
    });
}

fn finish_detail(detail: &mut RunDetail, code: i32, error: Option<String>) {
    detail.state = "stopped".to_string();
    detail.status = format!("stopped (exit {code})");
    detail.exit_code = Some(code);
    detail.ended_at = Some(now_ms());
    detail.error = error;
}

fn command_spec(command: &str, cwd: &std::path::Path) -> CommandSpec {
    let exec = default_shell_name();
    let args = match exec.to_lowercase().as_str() {
        "powershell" | "pwsh" | "powershell.exe" => vec![
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ],
        "cmd" | "cmd.exe" => vec!["/d", "/s", "/c", command],
        "node" => vec!["-e", command],
        "python" | "python3" => vec!["-c", command],
        _ => vec!["-lc", command],
    };
    CommandSpec::new(exec).args(args).cwd(cwd.to_path_buf())
}

fn cwd_for(options: &ExbashOptions, ctx: &ToolContext) -> PathBuf {
    options
        .workdir
        .as_ref()
        .map(|path| ctx.resolve(path))
        .unwrap_or_else(|| ctx.directory.clone())
}

fn next_id() -> String {
    format!(
        "rex-{}-{}",
        now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn default_shell_name() -> &'static str {
    if cfg!(windows) {
        "powershell.exe"
    } else {
        "bash"
    }
}

fn count_lines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
