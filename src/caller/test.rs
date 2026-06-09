use crate::{
    start_shared_executor_ws, Caller, ConnectExecutorOptions, Executor, ExecutorRequest,
    ShellManager,
};
use futures_util::{SinkExt, StreamExt};
use pty_t_protocol::{AdminText, ServerText};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;
use tokio::time::{timeout, Duration};
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
async fn caller_exposes_local_exit_events_only_through_binary_api() {
    let dir = tempdir().unwrap();
    let caller = Caller::new().await.unwrap();
    let response = caller
        .handle(ExecutorRequest {
            id: json!("local-exit-event"),
            method: "exbash".to_string(),
            params: json!({
                "mode": "run",
                "command": "sh -lc 'sleep 0.05; exit 6'",
                "read_timeout": 0
            }),
            directory: Some(dir.path().to_path_buf()),
            executor: Some("local".to_string()),
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    let result = response.result.unwrap();
    let async_id = result["metadata"]["asyncID"].as_str().unwrap();
    let mut rx = caller.subscribe_local_exit_code(async_id).unwrap();
    let code = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(code, 6);

    let detail = caller.local_exbash_run_detail(async_id).unwrap();
    assert_eq!(detail["asyncID"], json!(async_id));
    assert_eq!(detail["exitCode"], json!(6));
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
async fn caller_routes_remote_executor_across_multiple_directories() {
    let root = tempdir().unwrap();
    let alpha = root.path().join("alpha");
    let beta = root.path().join("beta");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(&beta).unwrap();
    fs::write(alpha.join("same.txt"), "alpha\n").unwrap();
    fs::write(beta.join("same.txt"), "beta\n").unwrap();

    let addr = start_shared_executor_ws(
        "127.0.0.1:0",
        Executor::local("remote-folders"),
        ShellManager::default_shell(80, 24),
    )
    .unwrap();
    let caller = Caller::new().await.unwrap();
    caller
        .connect_to_executor(ConnectExecutorOptions {
            id: "remote-folders".to_string(),
            url: format!("ws://{addr}"),
            system: Some("test".to_string()),
            device: Some("remote-device".to_string()),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();

    let alpha_response = caller
        .handle(ExecutorRequest {
            id: json!("alpha-read"),
            method: "read".to_string(),
            params: json!({"filePath":"same.txt"}),
            directory: Some(alpha.clone()),
            executor: Some("remote-folders".to_string()),
            tool_timeout_ms: None,
        })
        .await;
    assert!(alpha_response.ok, "{:?}", alpha_response.error);
    assert_eq!(alpha_response.executor.as_deref(), Some("remote-folders"));
    assert!(alpha_response.result.unwrap().to_string().contains("alpha"));

    let patch_response = caller
        .handle(ExecutorRequest {
            id: json!("beta-patch"),
            method: "FileAction".to_string(),
            params: json!({
                "mode":"patch",
                "filePath":"same.txt",
                "patchText":"1:BETA\n"
            }),
            directory: Some(beta.clone()),
            executor: Some("remote-folders".to_string()),
            tool_timeout_ms: None,
        })
        .await;
    assert!(patch_response.ok, "{:?}", patch_response.error);
    assert_eq!(
        fs::read_to_string(alpha.join("same.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(fs::read_to_string(beta.join("same.txt")).unwrap(), "BETA\n");
}

#[tokio::test]
async fn caller_allows_exbash_read_timeout_over_default_rpc_timeout() {
    let manager = ShellManager::default_shell(80, 24);
    let addr = start_shared_executor_ws(
        "127.0.0.1:0",
        Executor::local("remote-exbash-timeout"),
        manager,
    )
    .unwrap();

    let caller = Caller::new().await.unwrap();
    caller
        .connect_to_executor(ConnectExecutorOptions {
            id: "remote-exbash-timeout".to_string(),
            url: format!("ws://{addr}"),
            system: Some("test".to_string()),
            device: None,
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();

    let response = caller
        .handle(ExecutorRequest {
            id: json!("long-read-timeout"),
            method: "exbash".to_string(),
            params: json!({"mode":"run",
                "command":"echo long-timeout-ok",
                "read_timeout":31_000
            }),
            directory: None,
            executor: Some("remote-exbash-timeout".to_string()),
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    assert!(response
        .result
        .unwrap()
        .to_string()
        .contains("long-timeout-ok"));
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
