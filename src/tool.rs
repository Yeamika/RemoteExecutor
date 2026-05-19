use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub directory: PathBuf,
    pub worktree: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolResult {
    pub title: String,
    pub metadata: Value,
    pub output: String,
}

impl ToolContext {
    pub fn new(directory: Option<PathBuf>, worktree: Option<PathBuf>) -> Self {
        let directory = directory
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let worktree = worktree.unwrap_or_else(|| directory.clone());
        Self {
            directory,
            worktree,
        }
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
        path.strip_prefix(&self.worktree)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}
