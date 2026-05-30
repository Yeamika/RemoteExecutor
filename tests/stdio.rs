use remote_executor::{handle_request, run_stdio_io_with_caller, Caller, Executor, StdioRequest};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn stdio_dispatches_glob() {
    let request = StdioRequest {
        id: json!(1),
        method: "glob".to_string(),
        params: json!({"pattern":"*.rs"}),
        directory: None,
        executor: None,
        tool_timeout_ms: None,
    };

    let response = handle_request(request).await;
    assert_eq!(response.id, json!(1));
}

#[tokio::test]
async fn stdio_dispatches_apply_patch() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "before\n").unwrap();

    let request = StdioRequest {
        id: json!(2),
        method: "apply_patch".to_string(),
        params: json!({
            "filePath":"file.txt",
            "patchText":"replace 1 1\n+after"
        }),
        directory: Some(dir.path().to_path_buf()),
        executor: None,
        tool_timeout_ms: None,
    };

    let response = handle_request(request).await;
    assert!(response.ok);
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "after\n"
    );
}

#[tokio::test]
async fn executor_apply_patch_result_omits_full_file_contents() {
    let dir = tempdir().unwrap();
    let secret = "secret-line-that-should-not-be-returned-by-re";
    let path = dir.path().join("file.txt");
    fs::write(&path, long_file(secret)).unwrap();

    let response = Executor::local("patch-result")
        .handle(StdioRequest {
            id: json!("re"),
            method: "apply_patch".to_string(),
            params: json!({
                "filePath":"file.txt",
                "patchText":"replace 1 1\n+ONE"
            }),
            directory: Some(dir.path().to_path_buf()),
            executor: None,
            tool_timeout_ms: None,
        })
        .await;

    assert!(response.ok, "{:?}", response.error);
    assert_patch_result_omits_full_file(response.result.unwrap(), secret);
}

#[tokio::test]
async fn caller_apply_patch_result_omits_full_file_contents() {
    let dir = tempdir().unwrap();
    let secret = "secret-line-that-should-not-be-returned-by-rec";
    let path = dir.path().join("file.txt");
    fs::write(&path, long_file(secret)).unwrap();

    let response = handle_request(StdioRequest {
        id: json!("rec"),
        method: "apply_patch".to_string(),
        params: json!({
            "filePath":"file.txt",
            "patchText":"replace 1 1\n+ONE"
        }),
        directory: Some(dir.path().to_path_buf()),
        executor: None,
        tool_timeout_ms: None,
    })
    .await;

    assert!(response.ok, "{:?}", response.error);
    assert_patch_result_omits_full_file(response.result.unwrap(), secret);
}

fn long_file(secret: &str) -> String {
    let mut content = String::from("one\n");
    for idx in 0..40 {
        content.push_str(&format!("middle-{idx}\n"));
    }
    content.push_str(secret);
    content.push('\n');
    content
}

fn assert_patch_result_omits_full_file(result: serde_json::Value, secret: &str) {
    assert!(result["metadata"]["file"].get("before").is_none());
    assert!(result["metadata"]["file"].get("after").is_none());
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(
        !encoded.contains(secret),
        "patch result returned full file content"
    );
}

#[tokio::test]
async fn stdio_allows_concurrent_exbash_controls() {
    let caller = Caller::new().await.unwrap();
    let (mut input_tx, input_rx) = tokio::io::duplex(4096);
    let (output_tx, mut output_rx) = tokio::io::duplex(8192);

    let runner = tokio::spawn(async move {
        run_stdio_io_with_caller(caller, BufReader::new(input_rx), output_tx)
            .await
            .unwrap();
    });

    input_tx
        .write_all(
            br#"{"id":1,"tool":"exbash","params":{"command":"bash -lc 'sleep 0.2; echo first'","read_timeout":1000}}
{"id":2,"tool":"exbash","params":{"command":"echo second","read_timeout":1000}}
"#,
        )
        .await
        .unwrap();
    input_tx.shutdown().await.unwrap();

    let mut output = String::new();
    output_rx.read_to_string(&mut output).await.unwrap();
    runner.await.unwrap();

    let responses = output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses
            .iter()
            .filter(|response| response["ok"] == true)
            .count(),
        2
    );
    assert!(!responses.iter().any(|response| response["error"]
        .as_str()
        .unwrap_or("")
        .contains("write operation")));
}

#[tokio::test]
async fn stdio_keeps_requests_in_their_own_directories() {
    let first = tempdir().unwrap();
    let second = tempdir().unwrap();
    fs::write(first.path().join("same.txt"), "from first workspace\n").unwrap();
    fs::write(second.path().join("same.txt"), "from second workspace\n").unwrap();

    let caller = Caller::new().await.unwrap();
    let (mut input_tx, input_rx) = tokio::io::duplex(4096);
    let (output_tx, mut output_rx) = tokio::io::duplex(8192);

    let runner = tokio::spawn(async move {
        run_stdio_io_with_caller(caller, BufReader::new(input_rx), output_tx)
            .await
            .unwrap();
    });

    for request in [
        json!({
            "id":"first",
            "tool":"read",
            "directory":first.path(),
            "params":{"filePath":"same.txt"}
        }),
        json!({
            "id":"second",
            "tool":"read",
            "directory":second.path(),
            "params":{"filePath":"same.txt"}
        }),
    ] {
        input_tx
            .write_all(serde_json::to_string(&request).unwrap().as_bytes())
            .await
            .unwrap();
        input_tx.write_all(b"\n").await.unwrap();
    }
    input_tx.shutdown().await.unwrap();

    let mut output = String::new();
    output_rx.read_to_string(&mut output).await.unwrap();
    runner.await.unwrap();

    let responses = output
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .map(|response| (response["id"].as_str().unwrap().to_string(), response))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(responses.len(), 2);
    let first_response = responses.get("first").unwrap();
    let second_response = responses.get("second").unwrap();
    assert_eq!(first_response["ok"], true);
    assert_eq!(second_response["ok"], true);
    assert!(first_response["result"]["output"]
        .as_str()
        .unwrap()
        .contains("from first workspace"));
    assert!(second_response["result"]["output"]
        .as_str()
        .unwrap()
        .contains("from second workspace"));
}
