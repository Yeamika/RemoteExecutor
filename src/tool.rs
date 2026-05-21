use crate::shell_manager::ShellManager;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct ToolContext {
    pub directory: PathBuf,
    shell_manager: Option<ShellManager>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolResult {
    pub title: String,
    pub metadata: Value,
    pub output: String,
}

impl ToolContext {
    pub fn new(directory: Option<PathBuf>) -> Self {
        let directory = directory
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            directory,
            shell_manager: None,
        }
    }

    pub fn with_shell_manager(mut self, shell_manager: ShellManager) -> Self {
        self.shell_manager = Some(shell_manager);
        self
    }

    pub fn shell_manager(&self) -> Option<ShellManager> {
        self.shell_manager.clone()
    }

    pub fn resolve(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.directory.join(path)
        }
    }

    pub fn title(&self, path: &Path) -> String {
        path.strip_prefix(&self.directory)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}
