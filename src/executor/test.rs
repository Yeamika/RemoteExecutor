use crate::{
    hash_bytes, start_shared_executor_ws, Executor, ExecutorRequest, ExecutorResponse, ShellManager,
};
use futures_util::{SinkExt, StreamExt};
use pty_t_protocol::{AdminText, ClientText, ServerText};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn shared_endpoint_accepts_tool_and_pty_protocols() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "shared endpoint\n").unwrap();

    let manager = ShellManager::default_shell(80, 24);
    manager.create_bash("main").unwrap();
    let addr = start_shared_executor_ws("127.0.0.1:0", Executor::local("shared"), manager).unwrap();
    let url = format!("ws://{addr}");

    let (mut tool_ws, _) = connect_async(&url).await.unwrap();
    let tool_request = ExecutorRequest {
        id: json!(1),
        method: "read".to_string(),
        params: json!({"filePath":"file.txt"}),
        directory: Some(dir.path().to_path_buf()),
        executor: None,
        tool_timeout_ms: None,
    };
    tool_ws
        .send(Message::Text(
            serde_json::to_string(&tool_request).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(tool_response) = tool_ws.next().await.unwrap().unwrap() else {
        panic!("expected text tool response");
    };
    let tool_response: ExecutorResponse = serde_json::from_str(&tool_response).unwrap();
    assert!(tool_response.ok);
    assert_eq!(tool_response.executor.as_deref(), Some("shared"));

    let (mut pty_ws, _) = connect_async(&url).await.unwrap();
    pty_ws
        .send(Message::Text(
            serde_json::to_string(&AdminText::List).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(pty_response) = pty_ws.next().await.unwrap().unwrap() else {
        panic!("expected text pty response");
    };
    let pty_response: ServerText = serde_json::from_str(&pty_response).unwrap();
    let ServerText::Sessions { sessions } = pty_response else {
        panic!("expected pty sessions response");
    };
    assert!(sessions.iter().any(|session| session.pty == "main"));
}

#[tokio::test]
async fn shared_endpoint_reports_dedicated_file_transfer_url_and_transfers_files() {
    let dir = tempdir().unwrap();
    let source = b"download bytes\nwith binary-ish \x00 data\n".to_vec();
    fs::write(dir.path().join("source.bin"), &source).unwrap();

    let manager = ShellManager::default_shell(80, 24);
    let addr = start_shared_executor_ws(
        "127.0.0.1:0",
        Executor::local("shared-file-transfer"),
        manager,
    )
    .unwrap();
    let control_url = format!("ws://{addr}");

    let (mut control_ws, _) = connect_async(&control_url).await.unwrap();
    let info_request = ExecutorRequest {
        id: json!("info"),
        method: "_executor_info".to_string(),
        params: json!({}),
        directory: None,
        executor: None,
        tool_timeout_ms: None,
    };
    control_ws
        .send(Message::Text(
            serde_json::to_string(&info_request).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(info_response) = control_ws.next().await.unwrap().unwrap() else {
        panic!("expected executor info response");
    };
    let info_response: ExecutorResponse = serde_json::from_str(&info_response).unwrap();
    assert!(info_response.ok, "{:?}", info_response.error);
    let metadata = info_response.result.unwrap()["metadata"].clone();
    assert_eq!(metadata["protocol"], json!("remote-executor"));
    assert!(metadata["version"].as_str().unwrap_or_default().len() > 0);
    assert_eq!(metadata["capabilities"]["fileTransfer"], json!(true));
    assert_eq!(metadata["fileTransferPath"], json!("/re-file/v1"));
    let file_url = control_url
        .replacen("ws://", "http://", 1)
        .replace("/re-file/v1", "")
        + "/re-file/v1";
    assert_ne!(file_url, control_url);
    let (_, file_port, file_path) = parse_http_url(&file_url);
    let control_port = control_url
        .rsplit_once(':')
        .unwrap()
        .1
        .parse::<u16>()
        .unwrap();
    assert_eq!(file_port, control_port);
    assert_eq!(file_path, "/re-file/v1");

    assert!(file_url.starts_with("http://"), "{file_url}");
    let (status, download_headers, downloaded) = http_request(
        &file_url,
        &format!(
            "GET {{path}} HTTP/1.1\r\nX-RE-Directory: {}\r\nX-RE-Path: source.bin\r\nX-RE-Hash: true\r\nConnection: close\r\n\r\n",
            dir.path().display()
        ),
        &[],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(downloaded, source);
    assert_eq!(
        download_headers.get("x-re-sha256").map(String::as_str),
        Some(hash_bytes(&source).as_str())
    );

    let upload = b"uploaded payload\nsecond line\n".to_vec();
    let (status, _, upload_body) = http_request(
        &file_url,
        &format!(
            "PUT {{path}} HTTP/1.1\r\nX-RE-Directory: {}\r\nX-RE-Path: uploaded.bin\r\nX-RE-Sha256: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            dir.path().display(),
            hash_bytes(&upload),
            upload.len()
        ),
        &upload,
    )
    .await;
    assert_eq!(status, 200);
    let done: serde_json::Value = serde_json::from_slice(&upload_body).unwrap();
    assert_eq!(done["type"], json!("re.file.v1.done"));
    assert_eq!(done["sha256"], json!(hash_bytes(&upload)));
    assert_eq!(fs::read(dir.path().join("uploaded.bin")).unwrap(), upload);
}

#[tokio::test]
async fn shared_endpoint_exbash_sessions_are_visible_to_pty_clients() {
    let manager = ShellManager::default_shell(80, 24);
    manager.create_bash("main").unwrap();
    let addr =
        start_shared_executor_ws("127.0.0.1:0", Executor::local("shared-exbash"), manager).unwrap();
    let url = format!("ws://{addr}");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Write-Output visible; Start-Sleep -Seconds 1'"
    } else {
        "bash -lc 'printf visible; sleep 1'"
    };

    let (mut tool_ws, _) = connect_async(&url).await.unwrap();
    let request = ExecutorRequest {
        id: json!(10),
        method: "exbash".to_string(),
        params: json!({"mode":"run",
            "command": command,
            "description":"visible pty exbash",
            "read_timeout":0
        }),
        directory: None,
        executor: None,
        tool_timeout_ms: None,
    };
    tool_ws
        .send(Message::Text(
            serde_json::to_string(&request).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(response) = tool_ws.next().await.unwrap().unwrap() else {
        panic!("expected text tool response");
    };
    let response: ExecutorResponse = serde_json::from_str(&response).unwrap();
    assert!(response.ok, "{:?}", response.error);
    let async_id = response.result.unwrap()["metadata"]["asyncID"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut pty_ws, _) = connect_async(&url).await.unwrap();
    pty_ws
        .send(Message::Text(
            serde_json::to_string(&AdminText::List).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(pty_response) = pty_ws.next().await.unwrap().unwrap() else {
        panic!("expected text pty response");
    };
    let pty_response: ServerText = serde_json::from_str(&pty_response).unwrap();
    let ServerText::Sessions { sessions } = pty_response else {
        panic!("expected pty sessions response");
    };
    assert!(sessions.iter().any(|session| session.pty == async_id));

    let stop = ExecutorRequest {
        id: json!(11),
        method: "exbash".to_string(),
        params: json!({"mode":"stop","asyncID":async_id.clone()}),
        directory: None,
        executor: None,
        tool_timeout_ms: None,
    };
    tool_ws
        .send(Message::Text(serde_json::to_string(&stop).unwrap().into()))
        .await
        .unwrap();
    let _ = tool_ws.next().await.unwrap().unwrap();

    let remove = ExecutorRequest {
        id: json!(12),
        method: "exbash".to_string(),
        params: json!({"mode":"remove","asyncID":async_id}),
        directory: None,
        executor: None,
        tool_timeout_ms: None,
    };
    tool_ws
        .send(Message::Text(
            serde_json::to_string(&remove).unwrap().into(),
        ))
        .await
        .unwrap();
    let _ = tool_ws.next().await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn shared_endpoint_meta_reports_pty_exit_code() {
    let manager = ShellManager::default_shell(80, 24);
    let addr = start_shared_executor_ws(
        "127.0.0.1:0",
        Executor::local("shared-exit-status"),
        manager,
    )
    .unwrap();
    let url = format!("ws://{addr}");

    let (mut tool_ws, _) = connect_async(&url).await.unwrap();
    let request = ExecutorRequest {
        id: json!(20),
        method: "exbash".to_string(),
        params: json!({"mode":"run",
            "command":"bash -lc 'sleep 0.1; exit 7'",
            "read_timeout":0
        }),
        directory: None,
        executor: None,
        tool_timeout_ms: None,
    };
    tool_ws
        .send(Message::Text(
            serde_json::to_string(&request).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(response) = tool_ws.next().await.unwrap().unwrap() else {
        panic!("expected text tool response");
    };
    let response: ExecutorResponse = serde_json::from_str(&response).unwrap();
    assert!(response.ok, "{:?}", response.error);
    let async_id = response.result.unwrap()["metadata"]["asyncID"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut pty_ws, _) = connect_async(&url).await.unwrap();
    pty_ws
        .send(Message::Text(
            serde_json::to_string(&ClientText::Hello {
                id: "ptyt".to_string(),
                pty: async_id.clone(),
                cols: 80,
                rows: 24,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();

    let exit_code = timeout(Duration::from_secs(2), async {
        loop {
            let message = pty_ws.next().await?;
            let Ok(Message::Text(text)) = message else {
                continue;
            };
            let Ok(ServerText::Meta { exit_code, .. }) = serde_json::from_str(&text) else {
                continue;
            };
            if exit_code.is_some() {
                return exit_code;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(exit_code, Some(7));

    let (mut admin_ws, _) = connect_async(&url).await.unwrap();
    admin_ws
        .send(Message::Text(
            serde_json::to_string(&AdminText::List).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(pty_response) = admin_ws.next().await.unwrap().unwrap() else {
        panic!("expected text pty response");
    };
    let ServerText::Sessions { sessions } = serde_json::from_str(&pty_response).unwrap() else {
        panic!("expected pty sessions response");
    };
    let session = sessions
        .iter()
        .find(|session| session.pty == async_id)
        .unwrap();
    assert_eq!(session.exit_code, Some(7));
}

#[cfg(unix)]
#[tokio::test]
async fn exbash_mode_remove_closes_connected_pty_client() {
    let manager = ShellManager::default_shell(80, 24);
    let addr = start_shared_executor_ws(
        "127.0.0.1:0",
        Executor::local("shared-remove-close"),
        manager,
    )
    .unwrap();
    let url = format!("ws://{addr}");

    let (mut tool_ws, _) = connect_async(&url).await.unwrap();
    let request = ExecutorRequest {
        id: json!(30),
        method: "exbash".to_string(),
        params: json!({"mode":"run",
            "command":"bash -lc 'printf done; sleep 5'",
            "read_timeout":0
        }),
        directory: None,
        executor: None,
        tool_timeout_ms: None,
    };
    tool_ws
        .send(Message::Text(
            serde_json::to_string(&request).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(response) = tool_ws.next().await.unwrap().unwrap() else {
        panic!("expected text tool response");
    };
    let response: ExecutorResponse = serde_json::from_str(&response).unwrap();
    assert!(response.ok, "{:?}", response.error);
    let async_id = response.result.unwrap()["metadata"]["asyncID"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut pty_ws, _) = connect_async(&url).await.unwrap();
    pty_ws
        .send(Message::Text(
            serde_json::to_string(&ClientText::Hello {
                id: "ptyt".to_string(),
                pty: async_id.clone(),
                cols: 80,
                rows: 24,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();

    timeout(Duration::from_secs(2), async {
        loop {
            let Some(message) = pty_ws.next().await else {
                return;
            };
            let Ok(Message::Text(text)) = message else {
                continue;
            };
            let Ok(ServerText::Meta { .. }) = serde_json::from_str(&text) else {
                continue;
            };
            return;
        }
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let remove = ExecutorRequest {
        id: json!(31),
        method: "exbash".to_string(),
        params: json!({"mode":"remove","asyncID":async_id}),
        directory: None,
        executor: None,
        tool_timeout_ms: None,
    };
    tool_ws
        .send(Message::Text(
            serde_json::to_string(&remove).unwrap().into(),
        ))
        .await
        .unwrap();
    let Message::Text(response) = tool_ws.next().await.unwrap().unwrap() else {
        panic!("expected text tool response");
    };
    let response: ExecutorResponse = serde_json::from_str(&response).unwrap();
    assert!(response.ok, "{:?}", response.error);

    let close = timeout(Duration::from_secs(2), async {
        loop {
            let Some(message) = pty_ws.next().await else {
                return true;
            };
            match message.unwrap() {
                Message::Close(_) => return true,
                _ => continue,
            }
        }
    })
    .await
    .unwrap();
    assert!(close);
}

async fn http_request(
    url: &str,
    request_template: &str,
    body: &[u8],
) -> (u16, BTreeMap<String, String>, Vec<u8>) {
    let (host, port, path) = parse_http_url(url);
    let mut stream = TcpStream::connect(format!("{host}:{port}")).await.unwrap();
    let request = request_template.replace("{path}", &path).replacen(
        "\r\n",
        &format!("\r\nHost: {host}:{port}\r\n"),
        1,
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.shutdown().await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    parse_http_response(&response)
}

fn parse_http_url(url: &str) -> (String, u16, String) {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = rest.split_once('/').unwrap();
    let (host, port) = host_port.rsplit_once(':').unwrap();
    (host.to_string(), port.parse().unwrap(), format!("/{path}"))
}

fn parse_http_response(bytes: &[u8]) -> (u16, BTreeMap<String, String>, Vec<u8>) {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = headers_text.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let headers = lines
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    (status, headers, bytes[header_end + 4..].to_vec())
}
