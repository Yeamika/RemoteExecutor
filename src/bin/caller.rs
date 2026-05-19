use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    remote_executor::run_stdio().await
}
