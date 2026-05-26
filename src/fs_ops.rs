use crate::{rg_matches, RgOptions, ToolContext, ToolResult};
use anyhow::{anyhow, Context, Result};
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const DEFAULT_READ_LIMIT: usize = 2000;
const BINARY_READ_LIMIT: usize = 128;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";
const GLOB_LIMIT: usize = 100;
const GREP_LIMIT: usize = 100;

#[derive(Clone, Debug, Deserialize)]
pub struct GlobOptions {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

fn read_binary_file(
    path: &Path,
    title: String,
    file: FileStamp,
    offset: usize,
    limit: usize,
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
    let tail = if truncated {
        format!(
            "\n\n(Showing bytes {}-{} of {}. Use offset={} to continue.)",
            offset,
            end.saturating_sub(1),
            bytes.len(),
            end
        )
    } else {
        format!("\n\n(End of file - total {} bytes)", bytes.len())
    };
    let output = format!(
        "<path>{}</path>\n<type>binary</type>\n<content encoding=\"hex\" offset=\"{}\" length=\"{}\" total=\"{}\">\n{}{}\n</content>",
        path.display(),
        offset,
        slice.len(),
        bytes.len(),
        hex,
        tail
    );

    Ok(ToolResult {
        title,
        metadata: json!({
            "file": file,
            "mode": "binary",
            "encoding": "hex",
            "offset": offset,
            "length": slice.len(),
            "totalBytes": bytes.len(),
            "truncated": truncated,
            "preview": hex,
            "loaded": []
        }),
        output,
    })
}

#[derive(Clone, Debug, Deserialize)]
pub struct GrepOptions {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub include: Option<String>,
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
        title: ctx.title(&search),
        metadata: json!({ "count": files.len(), "truncated": truncated }),
        output: output.join("\n"),
    })
}

pub async fn grep_paths(options: GrepOptions, ctx: &ToolContext) -> Result<ToolResult> {
    if options.pattern.is_empty() {
        return Err(anyhow!("pattern is required"));
    }

    let search = options
        .path
        .as_ref()
        .map(|path| ctx.resolve(path))
        .unwrap_or_else(|| ctx.directory.clone());
    let mut rg = RgOptions::new(options.pattern.clone()).root(search.clone());
    if let Some(include) = options.include {
        rg = rg.glob(include);
    }
    let mut matches = rg_matches(rg).await?;
    matches.sort_by(|a, b| b.mod_time.cmp(&a.mod_time));

    let total = matches.len();
    let truncated = total > GREP_LIMIT;
    let final_matches = matches.into_iter().take(GREP_LIMIT).collect::<Vec<_>>();

    if final_matches.is_empty() {
        return Ok(ToolResult {
            title: options.pattern,
            metadata: json!({ "matches": 0, "truncated": false }),
            output: "No files found".to_string(),
        });
    }

    let mut output = vec![format!(
        "Found {total} matches{}",
        if truncated {
            format!(" (showing first {GREP_LIMIT})")
        } else {
            String::new()
        }
    )];
    let mut current = String::new();
    for hit in final_matches {
        if current != hit.path {
            if !current.is_empty() {
                output.push(String::new());
            }
            current = hit.path.clone();
            output.push(format!("{}:", hit.path));
        }
        let line = truncate_line(&hit.line);
        output.push(format!("  Line {}: {line}", hit.line_number));
    }
    if truncated {
        output.push(String::new());
        output.push(format!(
            "(Results truncated: showing {GREP_LIMIT} of {total} matches ({} hidden). Consider using a more specific path or pattern.)",
            total - GREP_LIMIT
        ));
    }

    Ok(ToolResult {
        title: options.pattern,
        metadata: json!({ "matches": total, "truncated": truncated }),
        output: output.join("\n"),
    })
}

pub fn read_path(options: ReadOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let filepath = ctx.resolve(&options.file_path);
    let title = ctx.title(&filepath);
    let stat = fs::metadata(&filepath)
        .with_context(|| format!("File not found: {}", filepath.display()))?;
    let file = file_stamp_for_metadata(&filepath, &stat)?;
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
            title,
            file,
            options.offset.unwrap_or(1),
            options.limit.unwrap_or(DEFAULT_READ_LIMIT),
        );
    }
    if mode == ReadMode::Binary {
        return read_binary_file(
            &filepath,
            title,
            file,
            options.offset.unwrap_or(0),
            options.limit.unwrap_or(BINARY_READ_LIMIT),
        );
    }
    read_file(
        &filepath,
        title,
        file,
        options.offset.unwrap_or(1),
        options.limit.unwrap_or(DEFAULT_READ_LIMIT),
    )
}

pub fn stat_path(options: StatOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let filepath = ctx.resolve(&options.file_path);
    let file = file_stamp(&filepath)?;
    Ok(ToolResult {
        title: ctx.title(&filepath),
        metadata: json!({ "file": file }),
        output: serde_json::to_string_pretty(&file)?,
    })
}

fn read_dir(
    path: &Path,
    title: String,
    file: FileStamp,
    offset: usize,
    limit: usize,
) -> Result<ToolResult> {
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
    let truncated = start + sliced.len() < entries.len();
    let tail = if truncated {
        format!(
            "\n(Showing {} of {} entries. Use 'offset' parameter to read beyond entry {})",
            sliced.len(),
            entries.len(),
            offset + sliced.len()
        )
    } else {
        format!("\n({} entries)", entries.len())
    };
    let output = format!(
        "<path>{}</path>\n<type>directory</type>\n<entries>\n{}{}\n</entries>",
        path.display(),
        sliced.join("\n"),
        tail
    );

    Ok(ToolResult {
        title,
        metadata: json!({ "file": file, "preview": sliced.iter().take(20).cloned().collect::<Vec<_>>().join("\n"), "truncated": truncated, "loaded": [] }),
        output,
    })
}

fn read_file(
    path: &Path,
    title: String,
    file: FileStamp,
    offset: usize,
    limit: usize,
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
    let mut output = format!(
        "<path>{}</path>\n<type>file</type>\n<content>",
        path.display()
    );
    for (idx, line) in raw.iter().enumerate() {
        output.push_str(&format!("\n{}: {line}", offset + idx));
    }
    if truncated {
        output.push_str(&format!(
            "\n\n(Showing lines {offset}-{last} of {}. Use offset={} to continue.)",
            lines.len(),
            last + 1
        ));
    } else {
        output.push_str(&format!("\n\n(End of file - total {} lines)", lines.len()));
    }
    output.push_str("\n</content>");

    Ok(ToolResult {
        title,
        metadata: json!({ "file": file, "preview": raw.iter().take(20).cloned().collect::<Vec<_>>().join("\n"), "truncated": truncated, "loaded": [] }),
        output,
    })
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
