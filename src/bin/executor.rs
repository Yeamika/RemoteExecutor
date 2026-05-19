use anyhow::Result;
use clap::Parser;
use pty_t_core::CommandSpec;
use remote_executor::{start_shared_executor_ws, Executor, ExecutorInfo, ShellManager};
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

    #[arg(long, default_value = "main")]
    pty: String,

    #[arg(long)]
    pty_program: Option<String>,
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
    let manager = ShellManager::default_shell(80, 24);
    manager.create_pty(
        args.pty.clone(),
        CommandSpec::new(args.pty_program.unwrap_or_else(default_program)),
        None,
        None,
    )?;
    let actual = start_shared_executor_ws(args.listen, executor, manager)?;
    println!("ws://{actual} pty={}", args.pty);

    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn default_program() -> String {
    if cfg!(windows) {
        "powershell.exe".to_string()
    } else {
        "bash".to_string()
    }
}
