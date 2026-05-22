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
pub use exec::{exbash, ExbashOptions, ExbashOutput};
pub use executor::{
    dispatch_tool, start_shared_executor_ws, Executor, ExecutorInfo, ExecutorRequest,
    ExecutorResponse,
};
pub use fs_ops::{glob_paths, grep_paths, read_path, GlobOptions, GrepOptions, ReadOptions};
pub use patch::{apply_diffy, apply_patch, ApplyOptions, DiffOptions, PatchFile};
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
}
