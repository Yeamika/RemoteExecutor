use anyhow::Result;
use clap::Parser;
use pty_t_core::CommandSpec;
use remote_executor::{
    start_shared_executor_ws, Executor, ExecutorInfo, SettingsStore, ShellManager,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

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

    #[arg(long)]
    settings: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let settings = SettingsStore::load(args.settings)?;
    let executor = Executor::new(ExecutorInfo {
        id: args.id,
        system: args
            .system
            .or_else(|| Some(std::env::consts::OS.to_string())),
        device: args.device.or_else(|| std::env::var("HOSTNAME").ok()),
        labels: BTreeMap::new(),
    })
    .with_settings_store(settings.clone());
    let manager = ShellManager::default_shell(80, 24);
    let pty_command = match args.pty_program {
        Some(program) => CommandSpec::new(program),
        None => settings.interactive_command_spec()?,
    };
    manager.create_pty(args.pty.clone(), pty_command, None, None)?;
    let actual = start_shared_executor_ws(args.listen, executor, manager)?;
    println!("ws://{actual} pty={}", args.pty);

    tokio::signal::ctrl_c().await?;
    Ok(())
}
