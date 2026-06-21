use crate::file_stamp;
use anyhow::{anyhow, Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

const HTTP_PATH: &str = "/re-file/v1";
const CHUNK_SIZE: usize = 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct FileTransferState {
    write_lock: Arc<Mutex<()>>,
}

impl FileTransferState {
    pub fn new(write_lock: Arc<Mutex<()>>) -> Self {
        Self { write_lock }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body_start: Vec<u8>,
}

pub async fn is_file_transfer_http_request(stream: &TcpStream) -> Result<bool> {
    let mut buf = [0u8; 256];
    for _ in 0..20 {
        let read = stream.peek(&mut buf).await?;
        if read == 0 {
            return Ok(false);
        }
        match file_transfer_request_prefix(&buf[..read]) {
            PrefixMatch::Yes => return Ok(true),
            PrefixMatch::No => return Ok(false),
            PrefixMatch::NeedMore => sleep(Duration::from_millis(5)).await,
        }
    }
    Ok(false)
}

pub async fn handle_file_transfer_http(
    mut stream: TcpStream,
    state: FileTransferState,
) -> Result<()> {
    let request = match read_http_request(&mut stream).await {
        Ok(request) => request,
        Err(err) => {
            write_json_response(
                &mut stream,
                400,
                json!({ "type": "re.file.v1.error", "message": err.to_string() }),
            )
            .await?;
            return Ok(());
        }
    };

    if request.path != HTTP_PATH {
        write_json_response(
            &mut stream,
            404,
            json!({ "type": "re.file.v1.error", "message": "unknown file transfer path" }),
        )
        .await?;
        return Ok(());
    }

    match request.method.as_str() {
        "GET" => handle_download(&mut stream, request).await,
        "PUT" | "POST" => handle_upload(&mut stream, request, state).await,
        _ => {
            write_json_response(
                &mut stream,
                405,
                json!({
                    "type": "re.file.v1.error",
                    "message": "use GET to download or PUT to upload"
                }),
            )
            .await
        }
    }
}

enum PrefixMatch {
    Yes,
    No,
    NeedMore,
}

fn file_transfer_request_prefix(bytes: &[u8]) -> PrefixMatch {
    let Some(line_end) = bytes.windows(2).position(|window| window == b"\r\n") else {
        return if is_possible_file_request_prefix(bytes) {
            PrefixMatch::NeedMore
        } else {
            PrefixMatch::No
        };
    };
    let line = &bytes[..line_end];
    for method in [b"GET ".as_slice(), b"PUT ".as_slice(), b"POST ".as_slice()] {
        if let Some(rest) = line.strip_prefix(method) {
            return if rest.starts_with(format!("{HTTP_PATH} ").as_bytes()) {
                PrefixMatch::Yes
            } else {
                PrefixMatch::No
            };
        }
    }
    PrefixMatch::No
}

fn is_possible_file_request_prefix(bytes: &[u8]) -> bool {
    [
        format!("GET {HTTP_PATH} "),
        format!("PUT {HTTP_PATH} "),
        format!("POST {HTTP_PATH} "),
    ]
    .iter()
    .any(|candidate| {
        candidate.as_bytes().starts_with(bytes) || bytes.starts_with(candidate.as_bytes())
    })
}

async fn handle_download(stream: &mut TcpStream, request: HttpRequest) -> Result<()> {
    let path = resolve_request_path(&request)?;
    let offset = parse_u64_header(&request.headers, "x-re-offset")?.unwrap_or(0);
    let want_hash = parse_bool_header(&request.headers, "x-re-hash")?.unwrap_or(false);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) => {
            write_error(
                stream,
                404,
                format!("failed to open {}: {err}", path.display()),
            )
            .await?;
            return Ok(());
        }
    };
    let metadata = file
        .metadata()
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if !metadata.is_file() {
        write_error(
            stream,
            400,
            format!("download only supports files: {}", path.display()),
        )
        .await?;
        return Ok(());
    }
    if offset > metadata.len() {
        write_error(
            stream,
            416,
            format!(
                "offset {offset} is out of range for this file ({} bytes)",
                metadata.len()
            ),
        )
        .await?;
        return Ok(());
    }

    let stamp = file_stamp(&path)?;
    let hash = if want_hash {
        Some(crate::file_hash_code(&path)?)
    } else {
        None
    };
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let remaining = metadata.len().saturating_sub(offset);
    let mut headers = vec![
        (
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        ),
        ("Content-Length".to_string(), remaining.to_string()),
        ("X-RE-Protocol".to_string(), "re.file.v1".to_string()),
        ("X-RE-Op".to_string(), "download".to_string()),
        (
            "X-RE-Path".to_string(),
            sanitize_header_value(&path.to_string_lossy()),
        ),
        ("X-RE-Offset".to_string(), offset.to_string()),
        ("X-RE-File-Size".to_string(), metadata.len().to_string()),
        (
            "X-RE-Canonical-Path".to_string(),
            sanitize_header_value(&stamp.canonical_path),
        ),
        (
            "X-RE-File-Key".to_string(),
            sanitize_header_value(&stamp.file_key),
        ),
    ];
    if let Some(mtime_ms) = stamp.mtime_ms {
        headers.push(("X-RE-Mtime-Ms".to_string(), mtime_ms.to_string()));
    }
    if let Some(hash) = hash {
        headers.push(("X-RE-Sha256".to_string(), hash));
    }
    write_response_head(stream, 200, &headers).await?;

    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let read = file.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        stream.write_all(&buf[..read]).await?;
    }
    stream.flush().await?;
    Ok(())
}

async fn handle_upload(
    stream: &mut TcpStream,
    request: HttpRequest,
    state: FileTransferState,
) -> Result<()> {
    let _guard = state.write_lock.lock().await;
    let path = resolve_request_path(&request)?;
    let content_length = match parse_content_length(&request.headers) {
        Ok(value) => value,
        Err(err) => {
            write_error(stream, 411, err.to_string()).await?;
            return Ok(());
        }
    };
    let overwrite = parse_bool_header(&request.headers, "x-re-overwrite")?.unwrap_or(false);
    let expected_sha = request.headers.get("x-re-sha256").cloned();
    if tokio::fs::try_exists(&path).await? && !overwrite {
        write_error(
            stream,
            409,
            format!(
                "target already exists; pass X-RE-Overwrite: true: {}",
                path.display()
            ),
        )
        .await?;
        return Ok(());
    }
    let parent = match path.parent() {
        Some(parent) => parent,
        None => {
            write_error(
                stream,
                400,
                format!("target path has no parent: {}", path.display()),
            )
            .await?;
            return Ok(());
        }
    };
    if !tokio::fs::try_exists(parent).await? {
        write_error(
            stream,
            404,
            format!("parent directory does not exist: {}", parent.display()),
        )
        .await?;
        return Ok(());
    }

    let temp_path = upload_temp_path(&path);
    let actual_sha = match receive_upload_body(stream, &request, &temp_path, content_length).await {
        Ok(actual_sha) => actual_sha,
        Err(err) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            write_error(stream, 400, err.to_string()).await?;
            return Ok(());
        }
    };
    if let Some(expected_sha) = expected_sha {
        if actual_sha != expected_sha {
            let _ = tokio::fs::remove_file(&temp_path).await;
            write_error(
                stream,
                409,
                format!("sha256 mismatch: expected {expected_sha}, got {actual_sha}"),
            )
            .await?;
            return Ok(());
        }
    }
    if overwrite && tokio::fs::try_exists(&path).await? {
        tokio::fs::remove_file(&path)
            .await
            .with_context(|| format!("failed to replace {}", path.display()))?;
    }
    tokio::fs::rename(&temp_path, &path)
        .await
        .with_context(|| format!("failed to move upload into {}", path.display()))?;
    let stamp = file_stamp(&path)?;

    write_json_response(
        stream,
        200,
        json!({
            "type": "re.file.v1.done",
            "op": "upload",
            "path": path.to_string_lossy(),
            "bytes": stamp.size.unwrap_or(0),
            "sha256": actual_sha,
            "file": stamp,
        }),
    )
    .await
}

async fn receive_upload_body(
    stream: &mut TcpStream,
    request: &HttpRequest,
    temp_path: &Path,
    content_length: u64,
) -> Result<String> {
    let mut temp = tokio::fs::File::create(temp_path)
        .await
        .with_context(|| format!("failed to create temp file {}", temp_path.display()))?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let initial_len = request.body_start.len().min(content_length as usize);
    if initial_len > 0 {
        let initial = &request.body_start[..initial_len];
        temp.write_all(initial).await?;
        hasher.update(initial);
        received += initial_len as u64;
    }
    if request.body_start.len() as u64 > content_length {
        return Err(anyhow!(
            "request body contains more bytes than Content-Length {content_length}"
        ));
    }

    let mut buf = vec![0u8; CHUNK_SIZE];
    while received < content_length {
        let remaining = (content_length - received).min(CHUNK_SIZE as u64) as usize;
        let read = stream.read(&mut buf[..remaining]).await?;
        if read == 0 {
            return Err(anyhow!(
                "connection closed after {received} bytes; expected {content_length}"
            ));
        }
        temp.write_all(&buf[..read]).await?;
        hasher.update(&buf[..read]);
        received += read as u64;
    }
    temp.flush().await?;
    Ok(sha256_from_digest(&hasher.finalize()))
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buf = Vec::<u8>::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(anyhow!("connection closed before request headers"));
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.len() > MAX_HEADER_BYTES {
            return Err(anyhow!("request headers exceed {MAX_HEADER_BYTES} bytes"));
        }
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
    };
    let head = std::str::from_utf8(&buf[..header_end])
        .map_err(|err| anyhow!("request headers must be UTF-8: {err}"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing HTTP request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP method"))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP path"))?
        .to_string();
    let version = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP version"))?;
    if !version.starts_with("HTTP/1.") {
        return Err(anyhow!("unsupported HTTP version: {version}"));
    }

    let mut headers = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(anyhow!("invalid HTTP header line: {line}"));
        };
        headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    Ok(HttpRequest {
        method,
        path,
        headers,
        body_start: buf[header_end + 4..].to_vec(),
    })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn resolve_request_path(request: &HttpRequest) -> Result<PathBuf> {
    let path = request
        .headers
        .get("x-re-path")
        .ok_or_else(|| anyhow!("X-RE-Path header is required"))
        .map(PathBuf::from)?;
    Ok(resolve_path(
        request.headers.get("x-re-directory").map(PathBuf::from),
        &path,
    ))
}

fn resolve_path(directory: Option<PathBuf>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        directory
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            .join(path)
    }
}

fn parse_content_length(headers: &BTreeMap<String, String>) -> Result<u64> {
    headers
        .get("content-length")
        .ok_or_else(|| anyhow!("Content-Length is required for upload"))?
        .parse::<u64>()
        .context("Content-Length must be an unsigned integer")
}

fn parse_u64_header(headers: &BTreeMap<String, String>, key: &str) -> Result<Option<u64>> {
    headers
        .get(key)
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("{key} must be an unsigned integer"))
        })
        .transpose()
}

fn parse_bool_header(headers: &BTreeMap<String, String>, key: &str) -> Result<Option<bool>> {
    headers
        .get(key)
        .map(|value| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" => Ok(false),
            _ => Err(anyhow!("{key} must be true or false")),
        })
        .transpose()
}

fn upload_temp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "upload".into());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        ".{name}.re-upload-{}-{now}.tmp",
        std::process::id()
    ))
}

async fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    value: serde_json::Value,
) -> Result<()> {
    let body = serde_json::to_vec(&value)?;
    write_response_head(
        stream,
        status,
        &[
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Content-Length".to_string(), body.len().to_string()),
        ],
    )
    .await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

async fn write_error(
    stream: &mut TcpStream,
    status: u16,
    message: impl Into<String>,
) -> Result<()> {
    write_json_response(
        stream,
        status,
        json!({
            "type": "re.file.v1.error",
            "message": message.into(),
        }),
    )
    .await
}

async fn write_response_head(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
) -> Result<()> {
    let reason = reason_phrase(status);
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n");
    for (key, value) in headers {
        head.push_str(key);
        head.push_str(": ");
        head.push_str(&sanitize_header_value(value));
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    Ok(())
}

fn sanitize_header_value(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

fn sha256_from_digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        416 => "Range Not Satisfiable",
        _ => "Error",
    }
}
