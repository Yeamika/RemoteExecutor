use remote_executor::{
    start_executor_ws, Caller, ConnectExecutorOptions, Executor, ExecutorRequest,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn caller_lists_local_executor() {
    let caller = Caller::new().await.unwrap();
    let response = caller
        .handle(ExecutorRequest {
            id: json!(1),
            method: "list_executor".to_string(),
            params: json!({}),
            directory: None,
            worktree: None,
            executor: None,
        })
        .await;

    assert!(response.ok);
    assert_eq!(response.executor.as_deref(), Some("caller"));
    assert!(response.result.unwrap().to_string().contains("local"));
}

#[tokio::test]
async fn caller_routes_to_connected_executor() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("remote.txt"), "hello from remote\n").unwrap();

    let addr = start_executor_ws("127.0.0.1:0", Executor::local("remote")).unwrap();
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
            worktree: Some(dir.path().to_path_buf()),
            executor: None,
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
