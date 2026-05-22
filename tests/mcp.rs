use remote_executor::{handle_mcp_message, run_mcp_stdio_io_with_caller, Caller};
use serde_json::{json, Value};
use std::fs;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn mcp_initialize_and_lists_tools() {
    let caller = Caller::new().await.unwrap();

    let initialized = handle_mcp_message(
        &caller,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await
    .unwrap();
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "remote-caller-mcp"
    );

    let listed = handle_mcp_message(
        &caller,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await
    .unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "read"));
    assert!(tools
        .iter()
        .any(|tool| tool["name"] == "connect_to_executor"));
    let read = tools.iter().find(|tool| tool["name"] == "read").unwrap();
    let properties = &read["inputSchema"]["properties"];
    assert!(properties.get("targetExecutor").is_some());
    assert!(properties.get("directory").is_some());
    assert!(properties.get("callTimeoutMs").is_none());

    let list = tools
        .iter()
        .find(|tool| tool["name"] == "list_executor")
        .unwrap();
    let list_properties = &list["inputSchema"]["properties"];
    assert!(list_properties.get("targetExecutor").is_none());
    assert!(list_properties.get("directory").is_none());

    let stop = tools
        .iter()
        .find(|tool| tool["name"] == "exbash_stop")
        .unwrap();
    let stop_properties = &stop["inputSchema"]["properties"];
    assert!(stop_properties.get("targetExecutor").is_some());
    assert!(stop_properties.get("directory").is_none());

    let exbash = tools.iter().find(|tool| tool["name"] == "exbash").unwrap();
    let exbash_properties = &exbash["inputSchema"]["properties"];
    assert!(exbash_properties.get("read_timeout").is_some());
    assert!(exbash_properties.get("async_timeout").is_none());
}

#[tokio::test]
async fn mcp_calls_caller_tool_over_stdio_shape() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "hello mcp\n").unwrap();
    let caller = Caller::new().await.unwrap();
    let dir_text = dir.path().to_string_lossy().to_string();

    let response = handle_mcp_message(
        &caller,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"read",
                "arguments":{
                    "filePath":"file.txt",
                    "directory":dir_text
                }
            }
        }),
    )
    .await
    .unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["result"]["isError"], Value::Bool(false));
    assert!(response["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("hello mcp"));
}

#[tokio::test]
async fn mcp_notifications_do_not_return_response() {
    let caller = Caller::new().await.unwrap();
    let response = handle_mcp_message(
        &caller,
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    )
    .await;
    assert!(response.is_none());
}

#[tokio::test]
async fn mcp_rejects_concurrent_writes() {
    let caller = Caller::new().await.unwrap();
    let (mut input_tx, input_rx) = tokio::io::duplex(4096);
    let (output_tx, mut output_rx) = tokio::io::duplex(8192);

    let runner = tokio::spawn(async move {
        run_mcp_stdio_io_with_caller(caller, BufReader::new(input_rx), output_tx)
            .await
            .unwrap();
    });

    input_tx
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"exbash","arguments":{"command":"sleep 0.2; echo first","read_timeout":1000}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"exbash","arguments":{"command":"echo second","read_timeout":1000}}}
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
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses
            .iter()
            .filter(|response| response["result"]["isError"] == false)
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|response| response["result"]["isError"] == true)
            .count(),
        1
    );
    assert!(responses
        .iter()
        .any(|response| response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("write operation")));
}
