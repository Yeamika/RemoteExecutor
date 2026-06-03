mod glob;
mod read;
mod stamp;
mod stat;
#[cfg(test)]
mod test;

use crate::{tool_output, tool_output_full, ToolContext, ToolResult};
use anyhow::{anyhow, Context, Result};
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const DEFAULT_READ_LIMIT: usize = 2000;
const BINARY_READ_LIMIT: usize = 128;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";
const GLOB_LIMIT: usize = 100;

#[derive(Clone, Debug, Deserialize)]
pub struct GlobOptions {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

fn read_binary_file(
    path: &Path,
    file: FileStamp,
    offset: usize,
    limit: usize,
    hash_code: Option<String>,
) -> Result<ToolResult> {
    let bytes = fs::read(path)?;
    if offset > bytes.len() {
        return Err(anyhow!(
            "Offset {offset} is out of range for this file ({} bytes)",
            bytes.len()
        ));
    }

    let limit = limit.clamp(1, BINARY_READ_LIMIT);
    let end = (offset + limit).min(bytes.len());
    let slice = &bytes[offset..end];
    let truncated = end < bytes.len();
    let hex = hexdump(slice, offset);

    let info = if truncated {
        format!(
            "Showing bytes {offset}-{} of {}. Use offset={end} to continue.",
            end.saturating_sub(1),
            bytes.len()
        )
    } else {
        format!("total {} bytes", bytes.len())
    };
    let message = "";
    let mut metadata = json!({ "file": file });
    if let Some(hash_code) = hash_code {
        metadata["hashCode"] = json!(hash_code);
    }

    Ok(ToolResult {
        metadata,
        output: tool_output_full(message, hex, info),
    })
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReadOptions {
    #[serde(rename = "filePath")]
    pub file_path: PathBuf,
    #[serde(default)]
    pub mode: Option<ReadMode>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default, rename = "hashCheckMode")]
    pub hash_check_mode: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReadMode {
    Text,
    Binary,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StatOptions {
    #[serde(rename = "filePath")]
    pub file_path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileStamp {
    #[serde(rename = "fileKey")]
    pub file_key: String,
    #[serde(rename = "canonicalPath")]
    pub canonical_path: String,
    pub kind: FileKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(rename = "mtimeMs", skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<u128>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Directory,
    Missing,
    Other,
}

pub fn glob_paths(options: GlobOptions, ctx: &ToolContext) -> Result<ToolResult> {
    if options.pattern.is_empty() {
        return Err(anyhow!("pattern is required"));
    }

    let search = options
        .path
        .as_ref()
        .map(|path| ctx.resolve(path))
        .unwrap_or_else(|| ctx.directory.clone());
    let globset = build_globset(&[options.pattern.clone()])?;
    let mut files = Vec::new();
    let mut truncated = false;

    for entry in WalkBuilder::new(&search).hidden(false).build() {
        let path = entry?.into_path();
        if !path.is_file() {
            continue;
        }
        let relative = path.strip_prefix(&search).unwrap_or(&path);
        if !globset.is_match(relative) && !globset.is_match(&path) {
            continue;
        }
        if files.len() >= GLOB_LIMIT {
            truncated = true;
            break;
        }
        let mtime = mtime_ms(&path);
        files.push((path, mtime));
    }

    files.sort_by(|a, b| b.1.cmp(&a.1));
    let mut output = Vec::new();
    if files.is_empty() {
        output.push("No files found".to_string());
    } else {
        output.extend(
            files
                .iter()
                .map(|(path, _)| path.to_string_lossy().into_owned()),
        );
        if truncated {
            output.push(String::new());
            output.push(format!(
                "(Results are truncated: showing first {GLOB_LIMIT} results. Consider using a more specific path or pattern.)"
            ));
        }
    }

    Ok(ToolResult {
        metadata: json!({ "count": files.len(), "truncated": truncated }),
        output: tool_output(output.join("\n")),
    })
}

pub fn read_path(options: ReadOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let filepath = ctx.resolve(&options.file_path);
    let stat = fs::metadata(&filepath)
        .with_context(|| format!("File not found: {}", filepath.display()))?;
    let file = file_stamp_for_metadata(&filepath, &stat)?;
    let hash_code = if options.hash_check_mode {
        if !stat.is_file() {
            return Err(anyhow!(
                "hashCheckMode only supports files: {}",
                filepath.display()
            ));
        }
        Some(file_hash_code(&filepath)?)
    } else {
        None
    };
    let mode = options.mode.unwrap_or(ReadMode::Text);
    if stat.is_dir() {
        if mode == ReadMode::Binary {
            return Err(anyhow!(
                "binary read mode only supports files: {}",
                filepath.display()
            ));
        }
        return read_dir(
            &filepath,
            file,
            options.offset.unwrap_or(1),
            options.limit.unwrap_or(DEFAULT_READ_LIMIT),
        );
    }
    if mode == ReadMode::Binary {
        return read_binary_file(
            &filepath,
            file,
            options.offset.unwrap_or(0),
            options.limit.unwrap_or(BINARY_READ_LIMIT),
            hash_code,
        );
    }
    read_file(
        &filepath,
        file,
        options.offset.unwrap_or(1),
        options.limit.unwrap_or(DEFAULT_READ_LIMIT),
        hash_code,
    )
}

pub fn stat_path(options: StatOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let filepath = ctx.resolve(&options.file_path);
    let file = file_stamp(&filepath)?;
    let output = format_file_stamp(&file);
    Ok(ToolResult {
        metadata: json!({ "file": file }),
        output: tool_output(output),
    })
}

fn format_file_stamp(file: &FileStamp) -> String {
    let kind = match file.kind {
        FileKind::File => "file",
        FileKind::Directory => "directory",
        FileKind::Missing => "missing",
        FileKind::Other => "other",
    };
    let mut lines = vec![
        format!("kind: {kind}"),
        format!("canonicalPath: {}", file.canonical_path),
        format!("fileKey: {}", file.file_key),
    ];
    if let Some(size) = file.size {
        lines.push(format!("size: {size}"));
    }
    if let Some(mtime_ms) = file.mtime_ms {
        lines.push(format!("mtimeMs: {mtime_ms}"));
    }
    lines.join("\n")
}

fn read_dir(path: &Path, file: FileStamp, offset: usize, limit: usize) -> Result<ToolResult> {
    if offset < 1 {
        return Err(anyhow!("offset must be greater than or equal to 1"));
    }
    let mut entries = fs::read_dir(path)?
        .map(|entry| {
            let entry = entry?;
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() {
                name.push('/');
            }
            Ok(name)
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort();

    let start = offset - 1;
    let limit = limit.max(1);
    let sliced = entries
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let total_entries = entries.len();
    let truncated = start + sliced.len() < total_entries;
    let next_offset = truncated.then_some(offset + sliced.len());
    let output = sliced.join("\n");

    let info = if truncated {
        format!(
            "Showing entries {offset}-{} of {total_entries}. Use offset={} to continue.",
            offset + sliced.len() - 1,
            next_offset.unwrap_or(offset)
        )
    } else {
        format!("total {total_entries} entries")
    };
    let message = "";
    Ok(ToolResult {
        metadata: json!({ "file": file }),
        output: tool_output_full(message, output, info),
    })
}

fn read_file(
    path: &Path,
    file: FileStamp,
    offset: usize,
    limit: usize,
    hash_code: Option<String>,
) -> Result<ToolResult> {
    if offset < 1 {
        return Err(anyhow!("offset must be greater than or equal to 1"));
    }
    let bytes = fs::read(path)?;
    if let Some((offset, byte)) = binary_byte(&bytes) {
        return Err(anyhow!(
            "Cannot read binary file: {} (binary byte at offset {}: 0x{:02X})",
            path.display(),
            offset,
            byte
        ));
    }

    let text = String::from_utf8_lossy(&bytes);
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() < offset && !(lines.is_empty() && offset == 1) {
        return Err(anyhow!(
            "Offset {offset} is out of range for this file ({} lines)",
            lines.len()
        ));
    }

    let start = offset - 1;
    let limit = limit.max(1);
    let raw = lines
        .iter()
        .skip(start)
        .take(limit)
        .map(|line| truncate_line(line))
        .collect::<Vec<_>>();
    let last = offset + raw.len().saturating_sub(1);
    let truncated = start + raw.len() < lines.len();
    let output = raw
        .iter()
        .enumerate()
        .map(|(idx, line)| format!("{}: {line}", offset + idx))
        .collect::<Vec<_>>()
        .join("\n");
    let next_offset = truncated.then_some(last + 1);

    let info = if truncated {
        format!(
            "Showing lines {offset}-{last} of {}. Use offset={} to continue.",
            lines.len(),
            next_offset.unwrap_or(last + 1)
        )
    } else {
        format!("total {} lines", lines.len())
    };
    let message = "";
    let mut metadata = json!({ "file": file });
    if let Some(hash_code) = hash_code {
        metadata["hashCode"] = json!(hash_code);
    }

    Ok(ToolResult {
        metadata,
        output: tool_output_full(message, output, info),
    })
}

pub fn file_hash_code(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(hash_bytes(&bytes))
}
pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_lower(&digest))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn file_stamp(path: &Path) -> Result<FileStamp> {
    match fs::metadata(path) {
        Ok(metadata) => file_stamp_for_metadata(path, &metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(FileStamp {
            file_key: format!("missing:{}", stable_path(path)?.display()),
            canonical_path: stable_path(path)?.to_string_lossy().into_owned(),
            kind: FileKind::Missing,
            size: None,
            mtime_ms: None,
        }),
        Err(err) => Err(err).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn file_stamp_for_metadata(path: &Path, metadata: &fs::Metadata) -> Result<FileStamp> {
    let kind = if metadata.is_file() {
        FileKind::File
    } else if metadata.is_dir() {
        FileKind::Directory
    } else {
        FileKind::Other
    };
    Ok(FileStamp {
        file_key: physical_file_key(path, metadata)?,
        canonical_path: stable_path(path)?.to_string_lossy().into_owned(),
        kind,
        size: metadata.is_file().then_some(metadata.len()),
        mtime_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis()),
    })
}

fn stable_path(path: &Path) -> Result<PathBuf> {
    Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

fn physical_file_key(path: &Path, metadata: &fs::Metadata) -> Result<String> {
    match file_id::get_file_id(path) {
        Ok(id) => Ok(format!("file-id:{id:?}")),
        Err(_) => Ok(format!(
            "path:{}:{}:{}",
            stable_path(path)?.display(),
            metadata.len(),
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or(0)
        )),
    }
}

fn build_globset(globs: &[String]) -> Result<globset::GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        builder.add(Glob::new(glob)?);
    }
    Ok(builder.build()?)
}

fn mtime_ms(path: &Path) -> u128 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn truncate_line(line: &str) -> String {
    if line.len() <= MAX_LINE_LENGTH {
        return line.to_string();
    }
    format!("{}{}", &line[..MAX_LINE_LENGTH], MAX_LINE_SUFFIX)
}

fn binary_byte(bytes: &[u8]) -> Option<(usize, u8)> {
    bytes
        .iter()
        .take(4096)
        .enumerate()
        .find_map(|(idx, byte)| (*byte == 0).then_some((idx, *byte)))
}

fn hexdump(bytes: &[u8], base: usize) -> String {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(row, chunk)| {
            let offset = base + row * 16;
            let hex = (0..16)
                .map(|idx| {
                    chunk
                        .get(idx)
                        .map(|byte| format!("{byte:02X}"))
                        .unwrap_or_else(|| "  ".to_string())
                })
                .collect::<Vec<_>>();
            let ascii = chunk
                .iter()
                .map(|byte| {
                    if byte.is_ascii_graphic() || *byte == b' ' {
                        *byte as char
                    } else {
                        '.'
                    }
                })
                .collect::<String>();
            format!("{offset:08X}  {}  |{}|", hex.join(" "), ascii)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
