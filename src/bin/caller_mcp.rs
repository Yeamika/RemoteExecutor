use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    remote_executor::run_mcp_stdio().await
}
