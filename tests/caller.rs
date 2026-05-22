use futures_util::{SinkExt, StreamExt};
use pty_t_protocol::{AdminText, ServerText};
use remote_executor::{
    start_shared_executor_ws, Caller, ConnectExecutorOptions, Executor, ExecutorRequest,
    ShellManager,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn caller_lists_local_executor() {
    let caller = Caller::new().await.unwrap();
    let response = caller
        .handle(ExecutorRequest {
            id: json!(1),
            method: "list_executor".to_string(),
            params: json!({}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok);
    assert_eq!(response.executor.as_deref(), Some("caller"));
    assert!(response.result.unwrap().to_string().contains("local"));
}

#[tokio::test]
async fn caller_local_executor_accepts_pty_protocol() {
    let caller = Caller::new().await.unwrap();
    let response = caller
        .handle(ExecutorRequest {
            id: json!(1),
            method: "list_executor".to_string(),
            params: json!({}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    let result = response.result.unwrap();
    let url = result["metadata"]["executors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|executor| executor["id"] == "local")
        .unwrap()["url"]
        .as_str()
        .unwrap();

    let (mut ws, _) = connect_async(url).await.unwrap();
    ws.send(Message::Text(
        serde_json::to_string(&AdminText::List).unwrap().into(),
    ))
    .await
    .unwrap();
    let Message::Text(response) = ws.next().await.unwrap().unwrap() else {
        panic!("expected pty admin response");
    };
    let response: ServerText = serde_json::from_str(&response).unwrap();
    let ServerText::Sessions { sessions } = response else {
        panic!("expected session list");
    };
    assert!(sessions.iter().any(|session| session.pty == "main"));
}

#[tokio::test]
async fn caller_routes_to_connected_executor() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("remote.txt"), "hello from remote\n").unwrap();

    let manager = ShellManager::default_shell(80, 24);
    let addr = start_shared_executor_ws("127.0.0.1:0", Executor::local("remote"), manager).unwrap();
    let caller = Caller::new().await.unwrap();
    caller
        .connect_to_executor(ConnectExecutorOptions {
            id: "remote".to_string(),
            url: format!("ws://{addr}"),
            system: Some("test".to_string()),
            device: Some("remote-device".to_string()),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();
    caller.set_default_executor("remote").await.unwrap();

    let response = caller
        .handle(ExecutorRequest {
            id: json!(2),
            method: "read".to_string(),
            params: json!({"filePath":"remote.txt"}),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok);
    assert_eq!(response.executor.as_deref(), Some("remote"));
    assert!(response
        .result
        .unwrap()
        .to_string()
        .contains("hello from remote"));
}
