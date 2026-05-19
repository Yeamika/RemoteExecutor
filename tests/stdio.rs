use remote_executor::{handle_request, StdioRequest};
use serde_json::json;

#[tokio::test]
async fn stdio_dispatches_glob() {
    let request = StdioRequest {
        id: json!(1),
        method: "glob".to_string(),
        params: json!({"pattern":"*.rs"}),
        directory: None,
        worktree: None,
    };

    let response = handle_request(request).await;
    assert_eq!(response.id, json!(1));
}
