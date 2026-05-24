use remote_executor::{Executor, ExecutorRequest};
use serde_json::json;
use std::fs;
use std::time::{Duration, Instant};
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
                "read_timeout":2000
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
async fn exbash_rejects_old_async_timeout_name() {
    let response = Executor::local("timeout")
        .handle(ExecutorRequest {
            id: json!(7),
            method: "exbash".to_string(),
            params: json!({
                "command":"echo hi",
                "async_timeout":0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(!response.ok);
    assert!(response.error.unwrap().contains("async_timeout"));
}

#[tokio::test]
async fn exbash_total_timeout_accepts_minus_one() {
    let executor = Executor::local("timeout");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Start-Sleep -Seconds 5'"
    } else {
        "sleep 5"
    };
    let start = executor
        .handle(ExecutorRequest {
            id: json!(9),
            method: "exbash".to_string(),
            params: json!({
                "command": command,
                "timeout": -1,
                "read_timeout": 0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(start.ok, "{:?}", start.error);
    let result = start.result.unwrap();
    assert_eq!(result["metadata"]["timeout"], json!(-1));
    let async_id = result["metadata"]["asyncID"].as_str().unwrap().to_string();

    let stop = executor
        .handle(ExecutorRequest {
            id: json!(10),
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
            id: json!(11),
            method: "exbash_remove".to_string(),
            params: json!({"asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
}

#[tokio::test]
async fn exbash_rejects_other_negative_total_timeouts() {
    let response = Executor::local("timeout")
        .handle(ExecutorRequest {
            id: json!(12),
            method: "exbash".to_string(),
            params: json!({
                "command":"echo hi",
                "timeout":-2,
                "read_timeout":0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(!response.ok);
    assert!(response.error.unwrap().contains("timeout must be -1"));
}

#[tokio::test]
async fn exbash_attach_waits_read_timeout_and_returns_snapshot() {
    let executor = Executor::local("attach-snapshot");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command '$line=[Console]::In.ReadLine(); Write-Output $line; Start-Sleep -Seconds 5'"
    } else {
        "bash -lc 'read line; echo $line; sleep 5'"
    };
    let start = executor
        .handle(ExecutorRequest {
            id: json!(3),
            method: "exbash".to_string(),
            params: json!({
                "command": command,
                "description":"snapshot attach",
                "read_timeout":0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(start.ok, "{:?}", start.error);
    let start_result = start.result.unwrap();
    assert_eq!(start_result["metadata"]["read_timeout"], json!(0));
    let async_id = start_result["metadata"]["asyncID"]
        .as_str()
        .unwrap()
        .to_string();

    let old_timeout = executor
        .handle(ExecutorRequest {
            id: json!(4),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID": async_id.clone(),
                "timeout":100
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(!old_timeout.ok);
    assert!(old_timeout.error.unwrap().contains("read_timeout"));

    let started = Instant::now();
    let attached = executor
        .handle(ExecutorRequest {
            id: json!(8),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID": async_id.clone(),
                "text":"hello snapshot\n",
                "read_timeout":100
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(attached.ok, "{:?}", attached.error);
    assert!(started.elapsed().as_millis() >= 90);

    let result = attached.result.unwrap();
    assert_eq!(result["metadata"]["read_timeout"], json!(100));
    assert!(result["metadata"]["outputBytes"].as_u64().unwrap() > 0);
    assert!(result["output"]
        .as_str()
        .unwrap()
        .contains("hello snapshot"));
    assert!(!result["output"].as_str().unwrap().contains("\u{1b}"));
    assert!(result["metadata"].get("rawPretty").is_none());

    let raw_pretty = executor
        .handle(ExecutorRequest {
            id: json!(17),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID": async_id.clone(),
                "read_timeout":0,
                "showRawPretty":true
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(raw_pretty.ok, "{:?}", raw_pretty.error);
    assert!(raw_pretty.result.unwrap()["metadata"]["rawPretty"].is_string());

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
    assert!(stop.result.unwrap()["output"]
        .as_str()
        .unwrap()
        .contains("hello snapshot"));

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
    assert_eq!(remove.result.unwrap()["output"], json!(""));
}

#[tokio::test]
async fn exbash_attach_returns_snapshot_for_stopped_run() {
    let executor = Executor::local("stopped-attach");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Start-Sleep -Milliseconds 100; Write-Output stopped-output'"
    } else {
        "bash -lc 'sleep 0.1; printf stopped-output'"
    };
    let start = executor
        .handle(ExecutorRequest {
            id: json!(13),
            method: "exbash".to_string(),
            params: json!({
                "command": command,
                "read_timeout": 0
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

    tokio::time::sleep(Duration::from_millis(250)).await;

    let attached = executor
        .handle(ExecutorRequest {
            id: json!(14),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID": async_id.clone(),
                "read_timeout": 1000
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(attached.ok, "{:?}", attached.error);
    let result = attached.result.unwrap();
    assert_eq!(result["metadata"]["state"], json!("stopped"));
    assert_eq!(result["metadata"]["inputFailed"], json!(false));
    assert_eq!(result["metadata"]["wrote"], json!(0));
    assert!(result["metadata"]["message"]
        .as_str()
        .unwrap()
        .contains("task already exited"));
    assert!(result["output"]
        .as_str()
        .unwrap()
        .contains("stopped-output"));

    let input_after_stop = executor
        .handle(ExecutorRequest {
            id: json!(15),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID": async_id.clone(),
                "text":"ignored\n",
                "read_timeout": 0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(input_after_stop.ok, "{:?}", input_after_stop.error);
    let result = input_after_stop.result.unwrap();
    assert_eq!(result["metadata"]["inputFailed"], json!(true));
    assert_eq!(result["metadata"]["source"], json!("text"));
    assert_eq!(result["metadata"]["wrote"], json!(0));
    assert!(result["metadata"]["message"]
        .as_str()
        .unwrap()
        .starts_with("input failed: task already exited"));
    assert!(result["output"]
        .as_str()
        .unwrap()
        .contains("stopped-output"));

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(16),
            method: "exbash_remove".to_string(),
            params: json!({"asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
}
