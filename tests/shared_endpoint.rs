use futures_util::{SinkExt, StreamExt};
use pty_t_protocol::{AdminText, ServerText};
use remote_executor::{
    start_shared_executor_ws, Executor, ExecutorRequest, ExecutorResponse, ShellManager,
};
use serde_json::json;
use std::fs;
use tempfile::tempdir;
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
        worktree: Some(dir.path().to_path_buf()),
        executor: None,
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
