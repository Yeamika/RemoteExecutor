use crate::exec::ExbashOptions;
use crate::ToolContext;
use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time;

const OUTPUT_LIMIT: usize = 30_000;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RunDetail {
    #[serde(rename = "asyncID")]
    pub(crate) async_id: String,
    pub(crate) scope: String,
    pub(crate) pid: Option<u32>,
    pub(crate) status: String,
    pub(crate) state: String,
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    #[serde(rename = "resultPath")]
    pub(crate) result_path: String,
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
    pub(crate) child: Mutex<Option<Child>>,
    pub(crate) detail: Mutex<RunDetail>,
    log: Mutex<tokio::fs::File>,
}

static JOBS: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) async fn start_job(options: &ExbashOptions, ctx: &ToolContext) -> Result<Arc<Job>> {
    let command = options
        .command
        .clone()
        .ok_or_else(|| anyhow!("command is required"))?;
    let cwd = cwd_for(options, ctx);
    let mut cmd = shell_command(&command, options.executor.as_deref());
    cmd.current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let id = next_id();
    let result_path = data_root().join(format!("{id}.log"));
    tokio::fs::create_dir_all(result_path.parent().unwrap()).await?;
    let log = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&result_path)
        .await?;
    let detail = RunDetail {
        async_id: id.clone(),
        scope: options.scope.clone().unwrap_or_else(|| "local".to_string()),
        pid: child.id(),
        status: "running".to_string(),
        state: "running".to_string(),
        exit_code: None,
        result_path: result_path.to_string_lossy().into_owned(),
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
        child: Mutex::new(Some(child)),
        detail: Mutex::new(detail),
        log: Mutex::new(log),
    });
    jobs().lock().await.insert(id, job.clone());
    spawn_pipe(stdout, job.clone());
    spawn_pipe(stderr, job.clone());
    if let Some(timeout) = options.timeout {
        spawn_timeout(job.clone(), timeout);
    }
    Ok(job)
}

pub(crate) async fn refresh_job(job: &Arc<Job>) -> Result<()> {
    if job.detail.lock().await.state == "stopped" {
        return Ok(());
    }
    let mut child = job.child.lock().await;
    if let Some(proc) = child.as_mut() {
        if let Some(status) = proc.try_wait()? {
            let mut detail = job.detail.lock().await;
            finish_detail(&mut detail, status.code().unwrap_or(-1), None);
            *child = None;
        }
    }
    Ok(())
}

pub(crate) async fn stop_job(job: &Arc<Job>, code: i32, error: Option<String>) -> Result<()> {
    let mut child = job.child.lock().await;
    if let Some(proc) = child.as_mut() {
        let _ = proc.start_kill();
        let _ = proc.wait().await;
    }
    *child = None;
    let mut detail = job.detail.lock().await;
    finish_detail(&mut detail, code, error);
    Ok(())
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
    path: &str,
    offset: u64,
    timeout: u64,
    window: usize,
) -> serde_json::Value {
    let end = now_ms() + u128::from(timeout);
    while now_ms() < end {
        let data = tokio::fs::read(path).await.unwrap_or_default();
        if data.len() as u64 > offset {
            let next = &data[offset as usize..];
            let start = next.len().saturating_sub(window);
            return json!({
                "output": String::from_utf8_lossy(&next[start..]),
                "bytes": next.len(),
                "overflow": next.len() > window,
                "timedOut": false,
            });
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    json!({ "output": "", "bytes": 0, "overflow": false, "timedOut": true })
}

pub(crate) fn jobs() -> &'static Mutex<HashMap<String, Arc<Job>>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn description(options: &ExbashOptions) -> String {
    options.description.clone().unwrap_or_else(|| {
        options
            .command
            .clone()
            .unwrap_or_else(|| options.mode.clone())
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

fn spawn_pipe<T>(stream: Option<T>, job: Arc<Job>)
where
    T: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut stream) = stream else { return };
        let mut buf = [0u8; 8192];
        loop {
            let n = match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if job.log.lock().await.write_all(&buf[..n]).await.is_ok() {
                job.detail.lock().await.line_pointer += count_lines(&buf[..n]);
            }
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

fn shell_command(command: &str, executor: Option<&str>) -> Command {
    let exec = executor.unwrap_or(default_shell_name());
    let mut cmd = Command::new(exec);
    match exec.to_lowercase().as_str() {
        "powershell" | "pwsh" | "powershell.exe" => {
            cmd.arg("-NoLogo")
                .arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-Command")
                .arg(command);
        }
        "cmd" | "cmd.exe" => {
            cmd.arg("/d").arg("/s").arg("/c").arg(command);
        }
        "node" => {
            cmd.arg("-e").arg(command);
        }
        "python" | "python3" => {
            cmd.arg("-c").arg(command);
        }
        _ => {
            cmd.arg("-lc").arg(command);
        }
    }
    cmd
}

fn cwd_for(options: &ExbashOptions, ctx: &ToolContext) -> PathBuf {
    options
        .workdir
        .as_ref()
        .map(|path| ctx.resolve(path))
        .unwrap_or_else(|| ctx.directory.clone())
}

fn data_root() -> PathBuf {
    std::env::var_os("REMOTE_EXECUTOR_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("remote-executor"))
        .join("exbash")
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
