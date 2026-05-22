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
async fn caller_rejects_non_canonical_names() {
    let caller = Caller::new().await.unwrap();
    for method in [
        "list_executors",
        "connect_executor",
        "set_def_executor",
        "exec",
    ] {
        let response = caller
            .handle(ExecutorRequest {
                id: json!(method),
                method: method.to_string(),
                params: json!({}),
                directory: None,
                executor: None,
                tool_timeout_ms: None,
            })
            .await;

        assert!(!response.ok, "{method} should not be accepted");
        assert!(response
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("unknown method"));
    }
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

#[tokio::test]
async fn caller_routes_to_multiple_executors() {
    let first_dir = tempdir().unwrap();
    let second_dir = tempdir().unwrap();
    fs::write(first_dir.path().join("same.txt"), "from first executor\n").unwrap();
    fs::write(second_dir.path().join("same.txt"), "from second executor\n").unwrap();

    let first_addr = start_shared_executor_ws(
        "127.0.0.1:0",
        Executor::local("first"),
        ShellManager::default_shell(80, 24),
    )
    .unwrap();
    let second_addr = start_shared_executor_ws(
        "127.0.0.1:0",
        Executor::local("second"),
        ShellManager::default_shell(80, 24),
    )
    .unwrap();

    let caller = Caller::new().await.unwrap();
    caller
        .connect_to_executor(ConnectExecutorOptions {
            id: "first".to_string(),
            url: format!("ws://{first_addr}"),
            system: Some("test".to_string()),
            device: Some("first-device".to_string()),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();
    caller
        .connect_to_executor(ConnectExecutorOptions {
            id: "second".to_string(),
            url: format!("ws://{second_addr}"),
            system: Some("test".to_string()),
            device: Some("second-device".to_string()),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();

    let first = caller
        .handle(ExecutorRequest {
            id: json!("first"),
            method: "read".to_string(),
            params: json!({"filePath":"same.txt"}),
            directory: Some(first_dir.path().to_path_buf()),
            executor: Some("first".to_string()),
            tool_timeout_ms: None,
        })
        .await;
    let second = caller
        .handle(ExecutorRequest {
            id: json!("second"),
            method: "read".to_string(),
            params: json!({"filePath":"same.txt"}),
            directory: Some(second_dir.path().to_path_buf()),
            executor: Some("second".to_string()),
            tool_timeout_ms: None,
        })
        .await;

    assert!(first.ok, "{:?}", first.error);
    assert!(second.ok, "{:?}", second.error);
    assert_eq!(first.executor.as_deref(), Some("first"));
    assert_eq!(second.executor.as_deref(), Some("second"));
    assert!(first
        .result
        .unwrap()
        .to_string()
        .contains("from first executor"));
    assert!(second
        .result
        .unwrap()
        .to_string()
        .contains("from second executor"));
}
