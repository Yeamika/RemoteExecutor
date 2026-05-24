use futures_util::{SinkExt, StreamExt};
use pty_t_protocol::{AdminText, ClientText, ServerText};
use remote_executor::{
    start_shared_executor_ws, Executor, ExecutorRequest, ExecutorResponse, ShellManager,
};
use serde_json::json;
use std::fs;
use tempfile::tempdir;
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
        params: json!({
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
        method: "exbash_stop".to_string(),
        params: json!({"asyncID":async_id.clone()}),
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
        method: "exbash_remove".to_string(),
        params: json!({"asyncID":async_id}),
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
        params: json!({
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
            let Some(message) = pty_ws.next().await else {
                return None;
            };
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
