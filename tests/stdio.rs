use remote_executor::{handle_request, StdioRequest};
use serde_json::json;
use std::fs;
use tempfile::tempdir;

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
