use remote_executor::{Executor, ExecutorRequest};
use serde_json::json;
use std::fs;
use std::time::Instant;
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
                "command":"echo hi",
                "description":"timeout smoke",
                "async_timeout":2000
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: Some(0),
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    assert!(response.result.unwrap().to_string().contains("hi"));
}

#[tokio::test]
async fn exbash_attach_waits_timeout_and_returns_snapshot() {
    let executor = Executor::local("attach-snapshot");
    let command = if cfg!(windows) {
        "$line=[Console]::In.ReadLine(); Write-Output $line; Start-Sleep -Seconds 5"
    } else {
        "read line; echo $line; sleep 5"
    };
    let start = executor
        .handle(ExecutorRequest {
            id: json!(3),
            method: "exbash".to_string(),
            params: json!({
                "command": command,
                "description":"snapshot attach",
                "async_timeout":0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(start.ok, "{:?}", start.error);
    let async_id = start.result.unwrap()["metadata"]["asyncID"]
        .as_str()
        .unwrap()
        .to_string();

    let started = Instant::now();
    let attached = executor
        .handle(ExecutorRequest {
            id: json!(4),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID": async_id,
                "text":"hello snapshot\n",
                "timeout":100
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(attached.ok, "{:?}", attached.error);
    assert!(started.elapsed().as_millis() >= 90);

    let result = attached.result.unwrap();
    assert!(result["metadata"]["outputBytes"].as_u64().unwrap() > 0);
    assert!(result["output"]
        .as_str()
        .unwrap()
        .contains("hello snapshot"));

    let stop = executor
        .handle(ExecutorRequest {
            id: json!(5),
            method: "exbash_stop".to_string(),
            params: json!({"asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(stop.ok, "{:?}", stop.error);

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(6),
            method: "exbash_remove".to_string(),
            params: json!({"asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
}
