mod caller;
mod context;
mod executor;
mod protocol;
mod settings;
mod shell_manager;
mod tools;
mod websocket;

pub use caller::{
    handle_mcp_message, handle_request, run_mcp_stdio, run_mcp_stdio_io_with_caller,
    run_mcp_stdio_with_caller, run_mcp_stdio_with_settings_path, run_stdio,
    run_stdio_io_with_caller, run_stdio_with_caller, run_stdio_with_settings_path, Caller,
    ConnectExecutorOptions, SetDefaultExecutorOptions, StdioRequest, StdioResponse,
};
pub use context::ToolContext;
pub use executor::{dispatch_tool, start_shared_executor_ws, Executor};
pub use protocol::{
    tool_output, tool_output_full, tool_output_with_info, ExecutorInfo, ExecutorRequest,
    ExecutorResponse, ToolResult,
};
pub use settings::{
    list_shells, set_default_shell, ListShellsOptions, ReSettings, SetDefaultShellOptions,
    SettingsStore, ShellProfile, ShellResolution, ShellSettings,
};
pub use shell_manager::ShellManager;
pub use tools::exbash::{exbash, exbash_run_detail, ExbashOptions, ExbashOutput};
pub use tools::file_action::{
    file_action, FileActionMode, FileActionOptions, PatchFile, PatchMode,
};
pub use tools::fs::{
    file_hash_code, file_stamp, glob_paths, hash_bytes, read_path, stat_path, FileKind, FileStamp,
    GlobOptions, ReadMode, ReadOptions, StatOptions,
};
pub use tools::rg::{rg_matches, rg_search, RgExecutor, RgMatch, RgOptions, RgOutput};

use anyhow::Result;
use std::path::PathBuf;

#[derive(Clone)]
pub struct RemoteExecutor {
    shell: ShellManager,
    rg: RgExecutor,
}

impl RemoteExecutor {
    pub fn new(shell: ShellManager, rg: RgExecutor) -> Self {
        Self { shell, rg }
    }

    pub fn default_shell(root: impl Into<PathBuf>, cols: u16, rows: u16) -> Self {
        Self {
            shell: ShellManager::default_shell(cols, rows),
            rg: RgExecutor::new(root),
        }
    }

    pub fn shell(&self) -> &ShellManager {
        &self.shell
    }

    pub fn rg(&self) -> &RgExecutor {
        &self.rg
    }

    pub async fn search(&self, options: RgOptions) -> Result<RgOutput> {
        self.rg.search(options).await
    }

    pub fn stat(&self, file_path: impl Into<PathBuf>) -> Result<FileStamp> {
        file_stamp(&file_path.into())
    }
}
