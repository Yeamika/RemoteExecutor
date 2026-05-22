use crate::{rg_matches, RgOptions, ToolContext, ToolResult};
use anyhow::{anyhow, Context, Result};
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_READ_LIMIT: usize = 2000;
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
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
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
    if stat.is_dir() {
        return read_dir(
            &filepath,
            title,
            options.offset.unwrap_or(1),
            options.limit.unwrap_or(DEFAULT_READ_LIMIT),
        );
    }
    read_file(
        &filepath,
        title,
        options.offset.unwrap_or(1),
        options.limit.unwrap_or(DEFAULT_READ_LIMIT),
    )
}

fn read_dir(path: &Path, title: String, offset: usize, limit: usize) -> Result<ToolResult> {
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
        metadata: json!({ "preview": sliced.iter().take(20).cloned().collect::<Vec<_>>().join("\n"), "truncated": truncated, "loaded": [] }),
        output,
    })
}

fn read_file(path: &Path, title: String, offset: usize, limit: usize) -> Result<ToolResult> {
    if offset < 1 {
        return Err(anyhow!("offset must be greater than or equal to 1"));
    }
    let bytes = fs::read(path)?;
    if is_binary(&bytes) {
        return Err(anyhow!("Cannot read binary file: {}", path.display()));
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
        metadata: json!({ "preview": raw.iter().take(20).cloned().collect::<Vec<_>>().join("\n"), "truncated": truncated, "loaded": [] }),
        output,
    })
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

fn is_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(4096)];
    sample.contains(&0)
}
