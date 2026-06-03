use crate::{settings::SettingsStore, shell_manager::ShellManager};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct ToolContext {
    pub directory: PathBuf,
    shell_manager: Option<ShellManager>,
    settings_store: Option<SettingsStore>,
}

impl ToolContext {
    pub fn new(directory: Option<PathBuf>) -> Self {
        let directory = directory
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            directory,
            shell_manager: None,
            settings_store: None,
        }
    }

    pub fn with_shell_manager(mut self, shell_manager: ShellManager) -> Self {
        self.shell_manager = Some(shell_manager);
        self
    }

    pub fn with_settings_store(mut self, settings_store: SettingsStore) -> Self {
        self.settings_store = Some(settings_store);
        self
    }

    pub fn settings_store(&self) -> Option<SettingsStore> {
        self.settings_store.clone()
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
