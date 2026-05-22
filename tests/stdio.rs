use remote_executor::{handle_request, run_stdio_io_with_caller, Caller, StdioRequest};
use serde_json::json;
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
            br#"{"id":1,"tool":"exbash","params":{"command":"sleep 0.2; echo first","async_timeout":1000}}
{"id":2,"tool":"exbash","params":{"command":"echo second","async_timeout":1000}}
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
