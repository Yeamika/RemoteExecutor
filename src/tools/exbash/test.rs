use crate::{Executor, ExecutorRequest, SettingsStore, ShellManager};
use serde_json::json;
use std::fs;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[tokio::test]
async fn executor_maps_rg_tool_timeout_to_soft_timeout() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "content\n".repeat(500_000)).unwrap();

    let response = Executor::local("timeout")
        .handle(ExecutorRequest {
            id: json!(1),
            method: "rg".to_string(),
            params: json!({"pattern":"needle", "root":dir.path().to_string_lossy()}),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: Some(0),
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    let result = response.result.unwrap();
    assert_eq!(result["metadata"]["timedOut"], json!(true));
}

#[tokio::test]
async fn executor_does_not_apply_tool_timeout_to_exbash() {
    let response = Executor::local("timeout")
        .handle(ExecutorRequest {
            id: json!(2),
            method: "exbash".to_string(),
            params: json!({"mode":"run",
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
async fn exbash_mode_shell_wraps_command_with_platform_shell() {
    let command = if cfg!(windows) {
        "Write-Output shell-ok"
    } else {
        "echo shell-ok"
    };
    let response = Executor::local("shell")
        .handle(ExecutorRequest {
            id: json!(35),
            method: "exbash".to_string(),
            params: json!({"mode":"shell",
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
async fn exbash_mode_shell_uses_hot_reloaded_default_shell() {
    let dir = tempdir().unwrap();
    let settings_path = dir.path().join(".re-setting.json");
    fs::write(&settings_path, shell_settings("one")).unwrap();

    let settings = SettingsStore::load(Some(settings_path.clone())).unwrap();
    let executor = Executor::local("shell-settings").with_settings_store(settings);

    let first = executor
        .handle(ExecutorRequest {
            id: json!("first-shell"),
            method: "exbash".to_string(),
            params: json!({"mode":"shell","command":"echo run", "read_timeout":2000}),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(first.ok, "{:?}", first.error);
    assert!(first.result.unwrap().to_string().contains("one-marker"));

    std::thread::sleep(Duration::from_millis(10));
    fs::write(&settings_path, shell_settings("two")).unwrap();

    let second = executor
        .handle(ExecutorRequest {
            id: json!("second-shell"),
            method: "exbash".to_string(),
            params: json!({"mode":"shell","command":"echo run", "read_timeout":2000}),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(second.ok, "{:?}", second.error);
    assert!(second
        .result
        .unwrap()
        .to_string()
        .contains("two-marker-extra"));
}

#[cfg(unix)]
#[tokio::test]
async fn set_default_shell_saves_settings_and_updates_default() {
    let dir = tempdir().unwrap();
    let settings_path = dir.path().join(".re-setting.json");
    fs::write(&settings_path, shell_settings("one")).unwrap();

    let settings = SettingsStore::load(Some(settings_path.clone())).unwrap();
    let executor = Executor::local("set-default-shell").with_settings_store(settings);
    let set = executor
        .handle(ExecutorRequest {
            id: json!("set-shell"),
            method: "set_default_shell".to_string(),
            params: json!({"shell":"two"}),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(set.ok, "{:?}", set.error);
    let set_output = set.result.as_ref().unwrap()["output"]["text"]
        .as_str()
        .unwrap();
    assert!(set_output.starts_with("defaultShell:two\nsettingsPath:"));
    assert!(set_output.contains("\nresolution: requested=two profile=two program=sh"));
    assert!(fs::read_to_string(&settings_path)
        .unwrap()
        .contains("\"default\": \"two\""));

    let run = executor
        .handle(ExecutorRequest {
            id: json!("run-shell"),
            method: "exbash".to_string(),
            params: json!({"mode":"shell","command":"echo run", "read_timeout":2000}),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(run.ok, "{:?}", run.error);
    assert!(run.result.unwrap().to_string().contains("two-marker-extra"));
}

#[cfg(unix)]
#[tokio::test]
async fn list_shells_returns_executor_settings() {
    let dir = tempdir().unwrap();
    let settings_path = dir.path().join(".re-setting.json");
    fs::write(&settings_path, shell_settings("one")).unwrap();

    let settings = SettingsStore::load(Some(settings_path.clone())).unwrap();
    let executor = Executor::local("list-shells").with_settings_store(settings);
    let response = executor
        .handle(ExecutorRequest {
            id: json!("list-shells"),
            method: "list_shells".to_string(),
            params: json!({}),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(response.ok, "{:?}", response.error);
    let result = response.result.unwrap();
    let output = result["output"]["text"].as_str().unwrap();
    assert!(output.starts_with("default:one\ninteractive:one\nsettingsPath:"));
    assert!(output.contains("profiles:\n- "));
    assert!(output.contains(
        "- one: candidates=sh commandArgs=-c echo one-marker; {command} interactiveArgs=<none>"
    ));
    assert_eq!(result["metadata"]["default"], "one");
    assert!(result["metadata"]["profiles"]["one"]["commandArgs"][1]
        .as_str()
        .unwrap()
        .contains("one-marker"));
    assert_eq!(
        result["metadata"]["settingsPath"].as_str().unwrap(),
        settings_path.to_string_lossy()
    );
}

#[cfg(unix)]
fn shell_settings(default_shell: &str) -> String {
    json!({
        "version": 1,
        "shells": {
            "default": default_shell,
            "interactive": "one",
            "profiles": {
                "one": {
                    "candidates": ["sh"],
                    "commandArgs": ["-c", "echo one-marker; {command}"],
                    "interactiveArgs": []
                },
                "two": {
                    "candidates": ["sh"],
                    "commandArgs": ["-c", "echo two-marker-extra; {command}"],
                    "interactiveArgs": []
                }
            }
        }
    })
    .to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn exbash_direct_does_not_use_shell_syntax() {
    let response = Executor::local("direct")
        .handle(ExecutorRequest {
            id: json!(36),
            method: "exbash".to_string(),
            params: json!({"mode":"run",
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
            params: json!({"mode":"run",
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
    assert!(result["output"]["text"]
        .as_str()
        .unwrap()
        .contains("before-detach"));
    assert!(result["metadata"].get("output").is_none());
    assert_eq!(
        result["output"]["message"],
        json!(format!("{async_id} detached"))
    );

    let _ = executor
        .handle(ExecutorRequest {
            id: json!(22),
            method: "exbash".to_string(),
            params: json!({"mode":"stop","asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    let _ = executor
        .handle(ExecutorRequest {
            id: json!(23),
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
}

#[tokio::test]
async fn exbash_mode_run_uses_workdir_param() {
    let dir = tempdir().unwrap();
    let subdir = dir.path().join("subdir");
    fs::create_dir(&subdir).unwrap();
    let executor = Executor::local("run-workdir");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command '(Get-Location).Path'"
    } else {
        "pwd"
    };

    let response = executor
        .handle(ExecutorRequest {
            id: json!(124),
            method: "exbash".to_string(),
            params: json!({"mode":"run", "command": command, "workdir":"subdir", "read_timeout":1000}),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    let result = response.result.unwrap();
    assert_eq!(result["metadata"]["exitCode"], json!(0));
    assert!(result["output"]["text"]
        .as_str()
        .unwrap()
        .contains(subdir.to_string_lossy().as_ref()));
}

#[tokio::test]
async fn exbash_detach_keeps_text_written_before_sleep() {
    let executor = Executor::local("detach-written-text");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Write-Output detached-text; Start-Sleep -Seconds 5'"
    } else {
        "bash -lc 'printf detached-text; sleep 5'"
    };

    let start = executor
        .handle(ExecutorRequest {
            id: json!(121),
            method: "exbash".to_string(),
            params: json!({"mode":"run", "command": command, "read_timeout":200}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(start.ok, "{:?}", start.error);
    let result = start.result.unwrap();
    let async_id = result["metadata"]["asyncID"].as_str().unwrap().to_string();
    assert_eq!(result["metadata"]["detached"], json!(true));
    assert!(result["output"]["text"]
        .as_str()
        .unwrap()
        .contains("detached-text"));
    assert_eq!(
        result["output"]["message"],
        json!(format!("{async_id} detached"))
    );

    let _ = executor
        .handle(ExecutorRequest {
            id: json!(122),
            method: "exbash".to_string(),
            params: json!({"mode":"stop","asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    let _ = executor
        .handle(ExecutorRequest {
            id: json!(123),
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
}

#[tokio::test]
async fn exbash_mode_remove_stops_running_process_before_removal() {
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
            params: json!({"mode":"run",
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
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
    let result = remove.result.unwrap();
    assert_eq!(result["metadata"]["ok"], json!(true));
    assert_eq!(result["output"]["text"], json!("ok"));

    let attached = executor
        .handle(ExecutorRequest {
            id: json!(26),
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
async fn exbash_total_timeout_sets_exit_code_to_timeout() {
    let executor = Executor::local("timeout-reason");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Start-Sleep -Seconds 5'"
    } else {
        "sleep 5"
    };

    let response = executor
        .handle(ExecutorRequest {
            id: json!(37),
            method: "exbash".to_string(),
            params: json!({"mode":"run",
                "command": command,
                "timeout":100,
                "read_timeout":2000
            }),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    assert_eq!(
        response.result.unwrap()["metadata"]["exitCode"],
        json!("timeout")
    );
}

#[tokio::test]
async fn exbash_mode_stop_sets_exit_code_to_stopped() {
    let executor = Executor::local("stop-reason");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Start-Sleep -Seconds 5'"
    } else {
        "sleep 5"
    };

    let start = executor
        .handle(ExecutorRequest {
            id: json!(38),
            method: "exbash".to_string(),
            params: json!({"mode":"run",
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

    let stop = executor
        .handle(ExecutorRequest {
            id: json!(39),
            method: "exbash".to_string(),
            params: json!({"mode":"stop","asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(stop.ok, "{:?}", stop.error);
    assert_eq!(
        stop.result.unwrap()["metadata"]["exitCode"],
        json!("stopped")
    );

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(40),
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
}

#[tokio::test]
async fn exbash_rejects_old_async_timeout_name() {
    let response = Executor::local("timeout")
        .handle(ExecutorRequest {
            id: json!(7),
            method: "exbash".to_string(),
            params: json!({"mode":"run",
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
            params: json!({"mode":"run","command":command}),
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
            params: json!({"mode":"run",
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
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
async fn exbash_mode_attach_rejects_oversized_file_input() {
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
            params: json!({"mode":"run",
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
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
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
            params: json!({"mode":"run",
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
            method: "exbash".to_string(),
            params: json!({"mode":"stop","asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(stop.ok, "{:?}", stop.error);

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(11),
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
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
            params: json!({"mode":"run",
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
            method: "exbash".to_string(),
            params: json!({"mode":"stop","asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(stop.ok, "{:?}", stop.error);

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(20),
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
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
            params: json!({"mode":"run",
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
async fn exbash_mode_attach_waits_read_timeout_and_returns_snapshot() {
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
            params: json!({"mode":"run",
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
    assert!(start_result["metadata"].get("read_timeout").is_none());
    let async_id = start_result["metadata"]["asyncID"]
        .as_str()
        .unwrap()
        .to_string();

    let old_timeout = executor
        .handle(ExecutorRequest {
            id: json!(4),
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
    assert!(result["metadata"].get("read_timeout").is_none());
    assert!(result["metadata"]["outputBytes"].as_u64().unwrap() > 0);
    assert!(result["output"]["text"]
        .as_str()
        .unwrap()
        .contains("hello snapshot"));
    assert!(!result["output"]["text"]
        .as_str()
        .unwrap()
        .contains("\u{1b}"));
    assert!(result["metadata"].get("rawPretty").is_none());

    let raw_pretty = executor
        .handle(ExecutorRequest {
            id: json!(17),
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
            method: "exbash".to_string(),
            params: json!({"mode":"stop","asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(stop.ok, "{:?}", stop.error);
    assert!(stop.result.unwrap()["output"]["text"]
        .as_str()
        .unwrap()
        .contains("hello snapshot"));

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(6),
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
    assert_eq!(remove.result.unwrap()["output"]["text"], json!("ok"));
}

#[tokio::test]
async fn exbash_mode_attach_returns_stopped_metadata_when_process_exits_during_wait() {
    let executor = Executor::local("attach-exits-during-wait");
    let command = if cfg!(windows) {
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command 'Start-Sleep -Milliseconds 50; Write-Output done'"
    } else {
        "bash -lc 'sleep 0.05; echo done'"
    };

    let start = executor
        .handle(ExecutorRequest {
            id: json!("start-exit-during-attach"),
            method: "exbash".to_string(),
            params: json!({"mode":"run", "command":command, "read_timeout":0}),
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
            id: json!("attach-exit-during-wait"),
            method: "exbash".to_string(),
            params: json!({"mode":"attach", "asyncID":async_id, "read_timeout":1000}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(attached.ok, "{:?}", attached.error);
    assert!(started.elapsed() < Duration::from_millis(900));

    let result = attached.result.unwrap();
    assert_eq!(result["metadata"]["state"], json!("stopped"));
    assert_eq!(result["metadata"]["exitCode"], json!(0));
    assert!(result["output"]["message"]
        .as_str()
        .unwrap()
        .contains("task exited with code 0"));
    assert!(result["metadata"].get("message").is_none());
    assert!(result["output"]["text"].as_str().unwrap().contains("done"));
}

#[cfg(unix)]
#[tokio::test]
async fn exbash_mode_attach_errors_with_controller_id_when_control_is_stolen() {
    let manager = ShellManager::default_shell(80, 24);
    let executor = Executor::local("control").with_shell_manager(manager.clone());
    let start = executor
        .handle(ExecutorRequest {
            id: json!(21),
            method: "exbash".to_string(),
            params: json!({"mode":"run",
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
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
            method: "exbash".to_string(),
            params: json!({"mode":"stop","asyncID":async_id.clone()}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(stop.ok, "{:?}", stop.error);

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(24),
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
}

#[tokio::test]
async fn exbash_mode_attach_returns_snapshot_for_stopped_run() {
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
            params: json!({"mode":"run",
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
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
    assert_eq!(result["metadata"]["wrote"], json!(0));
    assert!(result["output"]["message"]
        .as_str()
        .unwrap()
        .contains("task already exited"));
    assert!(result["metadata"].get("message").is_none());
    assert!(result["output"]["text"]
        .as_str()
        .unwrap()
        .contains("stopped-output"));

    let input_after_stop = executor
        .handle(ExecutorRequest {
            id: json!(15),
            method: "exbash".to_string(),
            params: json!({"mode":"attach",
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
    assert_eq!(result["metadata"]["source"], json!("text"));
    assert_eq!(result["metadata"]["wrote"], json!(0));
    assert!(result["output"]["message"]
        .as_str()
        .unwrap()
        .starts_with("input failed: task already exited"));
    assert!(result["metadata"].get("message").is_none());
    assert!(result["output"]["text"]
        .as_str()
        .unwrap()
        .contains("stopped-output"));

    let remove = executor
        .handle(ExecutorRequest {
            id: json!(16),
            method: "exbash".to_string(),
            params: json!({"mode":"remove","asyncID":async_id}),
            directory: None,
            executor: None,
            tool_timeout_ms: None,
        })
        .await;
    assert!(remove.ok, "{:?}", remove.error);
}
