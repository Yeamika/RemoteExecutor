use anyhow::Result;
use clap::Parser;
use remote_executor::{start_executor_ws, Executor, ExecutorInfo};
use std::collections::BTreeMap;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    id: String,

    #[arg(long, default_value = "127.0.0.1:0")]
    listen: String,

    #[arg(long)]
    system: Option<String>,

    #[arg(long)]
    device: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let executor = Executor::new(ExecutorInfo {
        id: args.id,
        system: args
            .system
            .or_else(|| Some(std::env::consts::OS.to_string())),
        device: args.device.or_else(|| std::env::var("HOSTNAME").ok()),
        labels: BTreeMap::new(),
    });
    let actual = start_executor_ws(args.listen, executor)?;
    println!("ws://{actual}");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
