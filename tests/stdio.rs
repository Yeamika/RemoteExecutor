use remote_executor::{handle_request, run_stdio_io_with_caller, Caller, StdioRequest};
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
async fn stdio_dispatches_diffy() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "before\n").unwrap();

    let request = StdioRequest {
        id: json!(2),
        method: "diffy".to_string(),
        params: json!({
            "patchText":"--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n"
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
async fn stdio_rejects_concurrent_writes() {
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
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|response| response["ok"] == false)
            .count(),
        1
    );
    assert!(responses.iter().any(|response| response["error"]
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
