mod caller;
mod exec;
mod exec_support;
mod executor;
mod fs_ops;
mod patch;
mod rg;
mod shell_manager;
mod tool;
mod websocket;

pub use caller::{
    handle_mcp_message, handle_request, run_mcp_stdio, run_mcp_stdio_io_with_caller,
    run_mcp_stdio_with_caller, run_stdio, run_stdio_io_with_caller, run_stdio_with_caller, Caller,
    ConnectExecutorOptions, SetDefaultExecutorOptions, StdioRequest, StdioResponse,
};
pub use exec::{exbash, exbash_shell, ExbashOptions, ExbashOutput};
pub use executor::{
    dispatch_tool, start_shared_executor_ws, Executor, ExecutorInfo, ExecutorRequest,
    ExecutorResponse,
};
pub use fs_ops::{
    file_hash_code, file_stamp, glob_paths, grep_paths, read_path, stat_path, FileKind, FileStamp,
    GlobOptions, GrepOptions, ReadMode, ReadOptions, StatOptions,
};
pub use patch::{apply_patch, ApplyOptions, PatchFile};
pub use rg::{rg_matches, rg_search, RgExecutor, RgMatch, RgOptions, RgOutput};
pub use shell_manager::ShellManager;
pub use tool::{ToolContext, ToolResult};

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
