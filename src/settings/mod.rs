mod shell;

use anyhow::{anyhow, Context, Result};
use pty_t_core::CommandSpec;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
    base_path: Option<PathBuf>,
    base_root: Option<PathBuf>,
    inner: Arc<Mutex<SettingsState>>,
}

#[derive(Clone, Debug)]
struct SettingsState {
    settings: ReSettings,
    stamp: Option<FileStamp>,
    base_stamp: Option<FileStamp>,
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

#[derive(Clone, Debug, Deserialize)]
pub struct ListShellsOptions {}

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
        Self::load_layered(None, path)
    }

    pub fn load_layered(base_path: Option<PathBuf>, path: PathBuf) -> Result<Self> {
        let path = absolutize(path)?;
        let base_path = base_path.map(absolutize).transpose()?;
        let base_path = base_path.filter(|base| base != &path);
        let root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let base_root = base_path
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let (settings, base_stamp, stamp) = load_settings_layers(base_path.as_deref(), &path)?;
        Ok(Self {
            path,
            root,
            base_path,
            base_root,
            inner: Arc::new(Mutex::new(SettingsState {
                settings,
                stamp,
                base_stamp,
            })),
        })
    }

    pub fn load_for_directory(directory: &Path) -> Result<Self> {
        Self::load_layered(Some(settings_path(None)?), directory.join(SETTINGS_FILE))
    }

    pub fn for_directory(&self, directory: &Path) -> Result<Self> {
        let base_path = self.base_path.clone().unwrap_or_else(|| self.path.clone());
        Self::load_layered(Some(base_path), directory.join(SETTINGS_FILE))
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
            base_path: None,
            base_root: None,
            inner: Arc::new(Mutex::new(SettingsState {
                settings: with_default_profiles(settings),
                stamp: None,
                base_stamp: None,
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
            if let Some(base_root) = &self.base_root {
                if let Some(program) = resolve_program(candidate, base_root) {
                    return Ok(program);
                }
            }
        }
        Err(anyhow!(
            "no candidate found for shell profile `{profile_name}`; checked: {}",
            profile.candidates.join(", ")
        ))
    }

    fn reload_if_changed(&self) -> Result<()> {
        let current = file_stamp(&self.path)?;
        let current_base = self
            .base_path
            .as_deref()
            .map(file_stamp)
            .transpose()?
            .flatten();
        let mut state = self.inner.lock().unwrap();
        if state.stamp == current && state.base_stamp == current_base {
            return Ok(());
        }
        let (settings, base_stamp, stamp) =
            load_settings_layers(self.base_path.as_deref(), &self.path)?;
        state.settings = settings;
        state.base_stamp = base_stamp;
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
    let output = format!(
        "defaultShell:{}\nsettingsPath:{}\nresolution: requested={} profile={} program={} args={} settingsPath={}",
        options.shell.trim(),
        settings.path().to_string_lossy(),
        resolution.requested,
        resolution.profile,
        resolution.program,
        display_args(&resolution.args),
        resolution.settings_path,
    );
    let metadata = json!({
        "defaultShell": options.shell.trim(),
        "resolution": resolution,
        "settingsPath": settings.path().to_string_lossy(),
    });
    Ok(crate::ToolResult {
        metadata: metadata.clone(),
        output: crate::tool_output(output),
    })
}

pub fn list_shells(
    _options: ListShellsOptions,
    ctx: &crate::ToolContext,
) -> Result<crate::ToolResult> {
    let settings = ctx
        .settings_store()
        .ok_or_else(|| anyhow!("settings store is not available"))?;
    let settings_path = settings.path().to_string_lossy().into_owned();
    let shell_settings = settings.settings()?.shells;
    let output = format_shell_settings(&shell_settings, &settings_path);
    let metadata = json!({
        "default": shell_settings.default,
        "interactive": shell_settings.interactive,
        "profiles": shell_settings.profiles,
        "settingsPath": settings_path,
    });
    Ok(crate::ToolResult {
        metadata: metadata.clone(),
        output: crate::tool_output(output),
    })
}

fn format_shell_settings(settings: &ShellSettings, settings_path: &str) -> String {
    let mut lines = vec![
        format!("default:{}", settings.default),
        format!("interactive:{}", settings.interactive),
        format!("settingsPath:{settings_path}"),
        "profiles:".to_string(),
    ];
    if settings.profiles.is_empty() {
        lines.push("- none".to_string());
    } else {
        lines.extend(settings.profiles.iter().map(|(name, profile)| {
            format!(
                "- {name}: candidates={} commandArgs={} interactiveArgs={}",
                display_args(&profile.candidates),
                display_args(&profile.command_args),
                display_args(&profile.interactive_args)
            )
        }));
    }
    lines.join("\n")
}

fn display_args(args: &[String]) -> String {
    if args.is_empty() {
        return "<none>".to_string();
    }
    args.iter()
        .map(|arg| {
            if arg.is_empty() {
                "<empty>".to_string()
            } else {
                arg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn load_settings_layers(
    base_path: Option<&Path>,
    path: &Path,
) -> Result<(ReSettings, Option<FileStamp>, Option<FileStamp>)> {
    let mut value = serde_json::to_value(ReSettings::default())?;
    let base_stamp = if let Some(base_path) = base_path {
        let (base, stamp) = load_settings_value(base_path)?;
        json_merge(&mut value, base);
        stamp
    } else {
        None
    };
    let (overlay, stamp) = load_settings_value(path)?;
    json_merge(&mut value, overlay);
    let settings = serde_json::from_value::<ReSettings>(value)
        .with_context(|| format!("failed to parse merged settings for {}", path.display()))?;
    Ok((with_default_profiles(settings), base_stamp, stamp))
}

fn load_settings_value(path: &Path) -> Result<(Value, Option<FileStamp>)> {
    if !path.exists() {
        return Ok((json!({}), None));
    }
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read settings file {}", path.display()))?;
    let value = serde_json::from_slice::<Value>(&bytes)
        .with_context(|| format!("failed to parse settings file {}", path.display()))?;
    Ok((value, file_stamp(path)?))
}

fn json_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                json_merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
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
    absolutize(path.unwrap_or_else(default_settings_path))
}

fn default_settings_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(SETTINGS_FILE)))
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE))
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
