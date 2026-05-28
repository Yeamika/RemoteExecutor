use remote_executor::{Executor, ExecutorRequest, ShellManager};
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
async fn exbash_shell_wraps_command_with_platform_shell() {
    let command = if cfg!(windows) {
        "Write-Output shell-ok"
    } else {
        "echo shell-ok"
    };
    let response = Executor::local("shell")
        .handle(ExecutorRequest {
            id: json!(35),
            method: "exbash_shell".to_string(),
            params: json!({
                "command":command,
                "read_timeout":2000
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    assert!(response.result.unwrap().to_string().contains("shell-ok"));
}

#[cfg(unix)]
#[tokio::test]
async fn exbash_direct_does_not_use_shell_syntax() {
    let response = Executor::local("direct")
        .handle(ExecutorRequest {
            id: json!(36),
            method: "exbash".to_string(),
            params: json!({
                "command":"echo direct-a; echo direct-b",
                "read_timeout":2000
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    let text = response.result.unwrap().to_string();
    assert!(text.contains("direct-a; echo direct-b"), "{text}");
    assert!(!text.contains("direct-b\r\n"), "{text}");
}

#[tokio::test]
async fn exbash_detach_returns_current_snapshot() {
    let executor = Executor::local("detach-snapshot");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Write-Output before-detach; Start-Sleep -Seconds 5'"
    } else {
        "bash -lc 'echo before-detach; sleep 5'"
    };

    let start = executor
        .handle(ExecutorRequest {
            id: json!(21),
            method: "exbash".to_string(),
            params: json!({
                "command": command,
                "description":"detach snapshot",
                "read_timeout":200
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(start.ok, "{:?}", start.error);
    let result = start.result.unwrap();
    let async_id = result["metadata"]["asyncID"].as_str().unwrap().to_string();
    assert_eq!(result["metadata"]["detached"], json!(true));
    assert!(result["output"].as_str().unwrap().contains("before-detach"));
    assert!(result["metadata"].get("output").is_none());

    let _ = executor
        .handle(ExecutorRequest {
            id: json!(22),
            method: "exbash_stop".to_string(),
            params: json!({"asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    let _ = executor
        .handle(ExecutorRequest {
            id: json!(23),
            method: "exbash_remove".to_string(),
            params: json!({"asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
}

#[tokio::test]
async fn exbash_remove_stops_running_process_before_removal() {
    let executor = Executor::local("remove-running");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Start-Sleep -Seconds 5'"
    } else {
        "sleep 5"
    };
    let start = executor
        .handle(ExecutorRequest {
            id: json!(24),
            method: "exbash".to_string(),
            params: json!({
                "command": command,
                "read_timeout":0
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

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(25),
            method: "exbash_remove".to_string(),
            params: json!({"asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
    let result = remove.result.unwrap();
    assert_eq!(result["metadata"]["removed"], json!(true));
    assert_eq!(result["metadata"]["stopped"], json!(true));
    assert_eq!(result["output"], json!(""));

    let attached = executor
        .handle(ExecutorRequest {
            id: json!(26),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID": async_id,
                "read_timeout":0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(!attached.ok);
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
async fn exbash_rejects_oversized_inputs() {
    let executor = Executor::local("input-limits");

    let command = "x".repeat(4097);
    let response = executor
        .handle(ExecutorRequest {
            id: json!(27),
            method: "exbash".to_string(),
            params: json!({"command":command}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("command exceeds 4096"));

    let description = "x".repeat(101);
    let response = executor
        .handle(ExecutorRequest {
            id: json!(28),
            method: "exbash".to_string(),
            params: json!({
                "command":"echo hi",
                "description":description
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("description exceeds 100"));

    let async_id = "x".repeat(31);
    let response = executor
        .handle(ExecutorRequest {
            id: json!(29),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID":async_id,
                "read_timeout":0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("asyncID exceeds 30"));

    let text = "x".repeat(4097);
    let response = executor
        .handle(ExecutorRequest {
            id: json!(30),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID":"rex-short",
                "text":text,
                "read_timeout":0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("text exceeds 4096"));

    let file_path = "x".repeat(4097);
    let response = executor
        .handle(ExecutorRequest {
            id: json!(31),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID":"rex-short",
                "filePath":file_path,
                "read_timeout":0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("filePath exceeds 4096"));
}

#[tokio::test]
async fn exbash_attach_rejects_oversized_file_input() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("payload.txt"), vec![b'x'; 4097]).unwrap();

    let executor = Executor::local("input-file-limit");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Start-Sleep -Seconds 5'"
    } else {
        "sleep 5"
    };
    let start = executor
        .handle(ExecutorRequest {
            id: json!(32),
            method: "exbash".to_string(),
            params: json!({
                "command": command,
                "read_timeout":0
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

    let attached = executor
        .handle(ExecutorRequest {
            id: json!(33),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID":async_id.clone(),
                "filePath":"payload.txt",
                "read_timeout":0
            }),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(!attached.ok);
    assert!(attached.error.unwrap().contains("file input exceeds 4096"));

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(34),
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
async fn exbash_total_timeout_accepts_zero_as_unlimited() {
    let executor = Executor::local("timeout");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Start-Sleep -Seconds 5'"
    } else {
        "sleep 5"
    };
    let start = executor
        .handle(ExecutorRequest {
            id: json!(18),
            method: "exbash".to_string(),
            params: json!({
                "command": command,
                "timeout": 0,
                "read_timeout": 0
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(start.ok, "{:?}", start.error);
    let result = start.result.unwrap();
    assert_eq!(result["metadata"]["timeout"], json!(0));
    let async_id = result["metadata"]["asyncID"].as_str().unwrap().to_string();

    let stop = executor
        .handle(ExecutorRequest {
            id: json!(19),
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
            id: json!(20),
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

#[cfg(unix)]
#[tokio::test]
async fn exbash_attach_errors_with_controller_id_when_control_is_stolen() {
    let manager = ShellManager::default_shell(80, 24);
    let executor = Executor::local("control").with_shell_manager(manager.clone());
    let start = executor
        .handle(ExecutorRequest {
            id: json!(21),
            method: "exbash".to_string(),
            params: json!({
                "command":"bash -lc 'read line; echo $line; sleep 5'",
                "read_timeout":0
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

    let manager_for_steal = manager.clone();
    let stolen_id = async_id.clone();
    let steal = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let session = manager_for_steal.core().session(&stolen_id).unwrap();
        let client = session
            .register_client("ptyt".to_string(), 1, 80, 24)
            .unwrap();
        session.set_controller(&client).unwrap();
    });

    let attached = executor
        .handle(ExecutorRequest {
            id: json!(22),
            method: "exbash_attach".to_string(),
            params: json!({
                "asyncID": async_id.clone(),
                "text":"hello stolen\n",
                "read_timeout":500
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    steal.await.unwrap();

    assert!(!attached.ok);
    let error = attached.error.unwrap();
    assert!(error.contains("someone attached"), "{error}");
    assert!(error.contains("ptyt"), "{error}");

    let stop = executor
        .handle(ExecutorRequest {
            id: json!(23),
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
            id: json!(24),
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
