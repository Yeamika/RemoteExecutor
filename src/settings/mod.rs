mod shell;

use anyhow::{anyhow, Context, Result};
use pty_t_core::CommandSpec;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

const SETTINGS_FILE: &str = ".re-setting.json";
const COMMAND_PLACEHOLDER: &str = "{command}";

#[derive(Clone)]
pub struct SettingsStore {
    path: PathBuf,
    root: PathBuf,
    inner: Arc<Mutex<SettingsState>>,
}

#[derive(Clone, Debug)]
struct SettingsState {
    settings: ReSettings,
    stamp: Option<FileStamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    mtime_ms: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReSettings {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub shells: ShellSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShellSettings {
    #[serde(default = "auto_string")]
    pub default: String,
    #[serde(default = "auto_string")]
    pub interactive: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, ShellProfile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShellProfile {
    #[serde(default)]
    pub candidates: Vec<String>,
    #[serde(default, rename = "commandArgs")]
    pub command_args: Vec<String>,
    #[serde(default, rename = "interactiveArgs")]
    pub interactive_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShellResolution {
    pub requested: String,
    pub profile: String,
    pub program: String,
    pub args: Vec<String>,
    #[serde(rename = "settingsPath")]
    pub settings_path: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SetDefaultShellOptions {
    pub shell: String,
}

impl Default for ReSettings {
    fn default() -> Self {
        Self {
            version: default_version(),
            shells: ShellSettings::default(),
        }
    }
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            default: auto_string(),
            interactive: auto_string(),
            profiles: default_profiles(),
        }
    }
}

impl SettingsStore {
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = settings_path(path)?;
        let root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let (settings, stamp) = load_settings_file(&path)?;
        Ok(Self {
            path,
            root,
            inner: Arc::new(Mutex::new(SettingsState { settings, stamp })),
        })
    }

    pub fn load_default_lossy() -> Self {
        Self::load(None).unwrap_or_else(|_| {
            let path = settings_path(None).unwrap_or_else(|_| PathBuf::from(SETTINGS_FILE));
            Self::from_settings(path, ReSettings::default())
        })
    }

    pub fn from_settings(path: PathBuf, settings: ReSettings) -> Self {
        let path = absolutize(path).unwrap_or_else(|_| PathBuf::from(SETTINGS_FILE));
        let root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            path,
            root,
            inner: Arc::new(Mutex::new(SettingsState {
                settings: with_default_profiles(settings),
                stamp: None,
            })),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn settings(&self) -> Result<ReSettings> {
        self.reload_if_changed()?;
        Ok(self.inner.lock().unwrap().settings.clone())
    }

    pub fn set_default_shell(&self, shell: &str) -> Result<ShellResolution> {
        let shell = shell.trim();
        if shell.is_empty() {
            return Err(anyhow!("shell is required"));
        }
        let resolution = self.resolve_shell(shell, None, false)?;
        {
            let mut state = self.inner.lock().unwrap();
            state.settings.shells.default = shell.to_string();
            state.stamp = write_settings_file(&self.path, &state.settings)?;
        }
        Ok(resolution)
    }

    pub fn command_spec(
        &self,
        shell: Option<&str>,
        command: &str,
        cwd: &Path,
    ) -> Result<CommandSpec> {
        let resolution = self.resolve_shell(shell.unwrap_or_default(), Some(command), false)?;
        Ok(CommandSpec::new(resolution.program)
            .args(resolution.args.iter().map(String::as_str))
            .cwd(cwd.to_path_buf()))
    }

    pub fn interactive_command_spec(&self) -> Result<CommandSpec> {
        let resolution = self.resolve_shell("", None, true)?;
        Ok(CommandSpec::new(resolution.program).args(resolution.args.iter().map(String::as_str)))
    }

    fn resolve_shell(
        &self,
        requested: &str,
        command: Option<&str>,
        interactive: bool,
    ) -> Result<ShellResolution> {
        self.reload_if_changed()?;
        let settings = self.inner.lock().unwrap().settings.clone();
        let requested = requested.trim();
        let configured = if requested.is_empty() {
            if interactive && !settings.shells.interactive.trim().is_empty() {
                settings.shells.interactive.as_str()
            } else {
                settings.shells.default.as_str()
            }
        } else {
            requested
        };
        let profile_name = resolve_profile_name(configured);
        let profile = settings.shells.profiles.get(&profile_name).ok_or_else(|| {
            anyhow!(
                "shell profile `{profile_name}` is not configured in {}",
                self.path.display()
            )
        })?;
        let program = self.resolve_candidate(&profile_name, profile)?;
        let args = if interactive {
            profile.interactive_args.clone()
        } else {
            expand_command_args(&profile.command_args, command.unwrap_or_default())
        };
        Ok(ShellResolution {
            requested: if requested.is_empty() {
                configured.to_string()
            } else {
                requested.to_string()
            },
            profile: profile_name,
            program,
            args,
            settings_path: self.path.to_string_lossy().into_owned(),
        })
    }

    fn resolve_candidate(&self, profile_name: &str, profile: &ShellProfile) -> Result<String> {
        for candidate in &profile.candidates {
            if let Some(program) = resolve_program(candidate, &self.root) {
                return Ok(program);
            }
        }
        Err(anyhow!(
            "no candidate found for shell profile `{profile_name}`; checked: {}",
            profile.candidates.join(", ")
        ))
    }

    fn reload_if_changed(&self) -> Result<()> {
        let current = file_stamp(&self.path)?;
        let mut state = self.inner.lock().unwrap();
        if state.stamp == current {
            return Ok(());
        }
        let (settings, stamp) = load_settings_file(&self.path)?;
        state.settings = settings;
        state.stamp = stamp;
        Ok(())
    }
}

pub fn set_default_shell(
    options: SetDefaultShellOptions,
    ctx: &crate::ToolContext,
) -> Result<crate::ToolResult> {
    let settings = ctx
        .settings_store()
        .ok_or_else(|| anyhow!("settings store is not available"))?;
    let resolution = settings.set_default_shell(&options.shell)?;
    let metadata = json!({
        "defaultShell": options.shell.trim(),
        "resolution": resolution,
        "settingsPath": settings.path().to_string_lossy(),
    });
    Ok(crate::ToolResult {
        metadata: metadata.clone(),
        output: crate::tool_output(serde_json::to_string_pretty(&metadata)?),
    })
}

fn load_settings_file(path: &Path) -> Result<(ReSettings, Option<FileStamp>)> {
    if !path.exists() {
        return Ok((ReSettings::default(), None));
    }
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read settings file {}", path.display()))?;
    let settings = serde_json::from_slice::<ReSettings>(&bytes)
        .with_context(|| format!("failed to parse settings file {}", path.display()))?;
    Ok((with_default_profiles(settings), file_stamp(path)?))
}

fn write_settings_file(path: &Path, settings: &ReSettings) -> Result<Option<FileStamp>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create settings directory {}", parent.display()))?;
    }
    fs::write(path, serde_json::to_vec_pretty(settings)?)
        .with_context(|| format!("failed to write settings file {}", path.display()))?;
    file_stamp(path)
}

fn settings_path(path: Option<PathBuf>) -> Result<PathBuf> {
    absolutize(path.unwrap_or_else(|| PathBuf::from(SETTINGS_FILE)))
}

fn absolutize(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn file_stamp(path: &Path) -> Result<Option<FileStamp>> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(None);
    };
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    Ok(Some(FileStamp {
        size: metadata.len(),
        mtime_ms,
    }))
}

fn with_default_profiles(mut settings: ReSettings) -> ReSettings {
    let defaults = default_profiles();
    for (name, profile) in defaults {
        settings.shells.profiles.entry(name).or_insert(profile);
    }
    if settings.shells.default.trim().is_empty() {
        settings.shells.default = auto_string();
    }
    if settings.shells.interactive.trim().is_empty() {
        settings.shells.interactive = auto_string();
    }
    settings
}

fn resolve_profile_name(name: &str) -> String {
    if name.trim().is_empty() || name.eq_ignore_ascii_case("auto") {
        if cfg!(windows) {
            "powershell".to_string()
        } else {
            "bash".to_string()
        }
    } else {
        name.to_string()
    }
}

fn expand_command_args(args: &[String], command: &str) -> Vec<String> {
    if args.iter().any(|arg| arg.contains(COMMAND_PLACEHOLDER)) {
        args.iter()
            .map(|arg| arg.replace(COMMAND_PLACEHOLDER, command))
            .collect()
    } else {
        let mut args = args.to_vec();
        args.push(command.to_string());
        args
    }
}

fn resolve_program(candidate: &str, root: &Path) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return None;
    }
    let path = PathBuf::from(candidate);
    if path.is_absolute() || has_path_separator(candidate) {
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        return path.exists().then(|| path.to_string_lossy().into_owned());
    }
    find_in_path(candidate).map(|_| candidate.to_string())
}

fn has_path_separator(value: &str) -> bool {
    value.contains('/') || value.contains('\\')
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for name in executable_names(program) {
            let path = dir.join(name);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

fn executable_names(program: &str) -> Vec<String> {
    if !cfg!(windows) || Path::new(program).extension().is_some() {
        return vec![program.to_string()];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut names = vec![program.to_string()];
    names.extend(
        pathext
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| format!("{program}{ext}")),
    );
    names
}

fn default_profiles() -> BTreeMap<String, ShellProfile> {
    BTreeMap::from([
        (
            "bash".to_string(),
            ShellProfile {
                candidates: vec![
                    ".venv/bin/bash".to_string(),
                    "bash".to_string(),
                    "/bin/bash".to_string(),
                    "sh".to_string(),
                ],
                command_args: vec!["-lc".to_string(), COMMAND_PLACEHOLDER.to_string()],
                interactive_args: vec!["-l".to_string()],
            },
        ),
        (
            "python".to_string(),
            ShellProfile {
                candidates: vec![
                    ".venv/bin/python".to_string(),
                    ".venv/Scripts/python.exe".to_string(),
                    "venv/bin/python".to_string(),
                    "venv/Scripts/python.exe".to_string(),
                    "python3".to_string(),
                    "python".to_string(),
                ],
                command_args: vec!["-c".to_string(), COMMAND_PLACEHOLDER.to_string()],
                interactive_args: Vec::new(),
            },
        ),
        (
            "node".to_string(),
            ShellProfile {
                candidates: vec!["node".to_string(), "nodejs".to_string()],
                command_args: vec!["-e".to_string(), COMMAND_PLACEHOLDER.to_string()],
                interactive_args: Vec::new(),
            },
        ),
        (
            "powershell".to_string(),
            ShellProfile {
                candidates: vec!["pwsh".to_string(), "powershell.exe".to_string()],
                command_args: vec![
                    "-NoLogo".to_string(),
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    COMMAND_PLACEHOLDER.to_string(),
                ],
                interactive_args: vec!["-NoLogo".to_string()],
            },
        ),
    ])
}

fn default_version() -> u32 {
    1
}

fn auto_string() -> String {
    "auto".to_string()
}
