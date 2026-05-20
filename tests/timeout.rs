use remote_executor::{Executor, ExecutorRequest};
use serde_json::json;
use std::fs;
use tempfile::tempdir;

#[tokio::test]
async fn executor_applies_tool_timeout_to_small_tools() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "needle\n").unwrap();

    let response = Executor::local("timeout")
        .handle(ExecutorRequest {
            id: json!(1),
            method: "grep".to_string(),
            params: json!({"pattern":"needle"}),
            directory: Some(dir.path().to_path_buf()),
            worktree: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: Some(0),
        })
        .await;

    assert!(!response.ok);
    assert!(response.error.unwrap().contains("timed out"));
}

#[tokio::test]
async fn executor_does_not_apply_tool_timeout_to_exbash() {
    let response = Executor::local("timeout")
        .handle(ExecutorRequest {
            id: json!(2),
            method: "exbash".to_string(),
            params: json!({
                "mode":"exec_timeout_async",
                "command":"echo hi",
                "description":"timeout smoke",
                "async_timeout":2000
            }),
            directory: None,
            worktree: None,
            executor: None,
            tool_timeout_ms: Some(0),
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    assert!(response.result.unwrap().to_string().contains("hi"));
}
