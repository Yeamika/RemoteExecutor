mod binary_patch;
mod options;
mod result;
#[cfg(test)]
mod test;
mod text_patch;

use crate::{hash_bytes, tool_output, ToolContext, ToolResult};
use anyhow::{anyhow, Context, Result};
use diffy::create_patch as diffy_create_patch;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
pub struct FileActionOptions {
    pub mode: FileActionMode,
    #[serde(rename = "filePath")]
    pub file_path: PathBuf,
    #[serde(default, rename = "newFilePath")]
    pub new_file_path: Option<PathBuf>,
    #[serde(default, rename = "patchText")]
    pub patch_text: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, rename = "patchMode")]
    pub patch_mode: PatchMode,
    #[serde(default, rename = "hashCheckMode")]
    pub hash_check_mode: bool,
    #[serde(default, rename = "hashCode")]
    pub hash_code: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileActionMode {
    Patch,
    Create,
    Delete,
    Rename,
}

fn binary_patch_file(
    ctx: &ToolContext,
    path: &Path,
    additions: usize,
    deletions: usize,
) -> PatchFile {
    PatchFile {
        file_path: path.to_string_lossy().into_owned(),
        relative_path: ctx.title(path),
        new_file_path: None,
        new_relative_path: None,
        kind: "binary-update".to_string(),
        additions,
        deletions,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PatchMode {
    #[default]
    Text,
    Binary,
}

#[derive(Clone, Debug, Serialize)]
pub struct PatchFile {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "relativePath")]
    pub relative_path: String,
    #[serde(rename = "newFilePath", skip_serializing_if = "Option::is_none")]
    pub new_file_path: Option<String>,
    #[serde(rename = "newRelativePath", skip_serializing_if = "Option::is_none")]
    pub new_relative_path: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Clone, Debug)]
struct TextShape {
    bom: bool,
    line_ending: &'static str,
    text: String,
}

#[derive(Clone, Debug)]
struct BinaryHunk {
    anchor: BinaryAnchor,
    bytes: Vec<u8>,
    order: usize,
}

#[derive(Clone, Debug)]
enum BinaryAnchor {
    Replace {
        offset: usize,
        len: usize,
    },
    Delete {
        offset: usize,
        len: usize,
    },
    Append {
        target: BinaryAppendTarget,
        len: usize,
    },
}

#[derive(Clone, Debug)]
enum BinaryAppendTarget {
    Offset(usize),
    End,
}

#[derive(Clone, Debug)]
struct BinaryOperation {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
    order: usize,
    additions: usize,
    deletions: usize,
}

#[derive(Clone, Debug)]
struct BinaryPatchApplied {
    bytes: Vec<u8>,
    additions: usize,
    deletions: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LinePatchOp {
    Delete {
        start: usize,
        end: usize,
    },
    Move {
        start: usize,
        end: usize,
        after: LineInsertTarget,
    },
    Append {
        after: LineInsertTarget,
        lines: Vec<String>,
    },
    Replace {
        line: usize,
        text: String,
    },
}

#[derive(Clone, Debug)]
struct PendingLineInsertion {
    boundary: usize,
    order: usize,
    kind: PendingLineInsertionKind,
}

#[derive(Clone, Debug)]
enum PendingLineInsertionKind {
    Literal(Vec<String>),
    Move { start: usize, end: usize },
}

#[derive(Clone, Debug)]
struct LineInsertion {
    order: usize,
    lines: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineRemoval {
    Delete,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineInsertTarget {
    Start,
    After(usize),
    End,
}

pub async fn file_action(options: FileActionOptions, ctx: &ToolContext) -> Result<ToolResult> {
    let target = ctx.resolve(&options.file_path);
    match options.mode {
        FileActionMode::Patch => patch_file_action(ctx, &target, &options).await,
        FileActionMode::Create => create_file_action(ctx, &target, &options).await,
        FileActionMode::Delete => delete_file_action(ctx, &target, &options).await,
        FileActionMode::Rename => rename_file_action(ctx, &target, &options).await,
    }
}

async fn patch_file_action(
    ctx: &ToolContext,
    target: &Path,
    options: &FileActionOptions,
) -> Result<ToolResult> {
    let patch_text = options
        .patch_text
        .as_deref()
        .ok_or_else(|| anyhow!("patchText is required for mode=patch"))?;
    if patch_text.trim().is_empty() {
        return Err(anyhow!("patchText is required for mode=patch"));
    }
    let before_bytes = read_existing_with_hash_check(target, options)?;
    match options.patch_mode {
        PatchMode::Text => apply_text_patch(ctx, target, before_bytes, patch_text, options).await,
        PatchMode::Binary => {
            apply_binary_patch(ctx, target, before_bytes, patch_text, options).await
        }
    }
}

async fn create_file_action(
    ctx: &ToolContext,
    target: &Path,
    options: &FileActionOptions,
) -> Result<ToolResult> {
    if target.exists() {
        return Err(anyhow!("cannot create existing file: {}", target.display()));
    }
    let content = options
        .content
        .as_deref()
        .ok_or_else(|| anyhow!("content is required for mode=create"))?;
    let bytes = match options.patch_mode {
        PatchMode::Text => content.as_bytes().to_vec(),
        PatchMode::Binary => decode_hex(content)?,
    };
    fs::write(target, &bytes)
        .with_context(|| format!("failed to create file {}", target.display()))?;
    let file = action_file(ctx, target, "create", bytes.len(), 0);
    Ok(result_from_file(
        file,
        options.hash_check_mode.then_some(hash_bytes(&bytes)),
    ))
}

async fn delete_file_action(
    ctx: &ToolContext,
    target: &Path,
    options: &FileActionOptions,
) -> Result<ToolResult> {
    let before = read_existing_with_hash_check(target, options)?;
    fs::remove_file(target)
        .with_context(|| format!("failed to delete file {}", target.display()))?;
    let file = action_file(ctx, target, "delete", 0, before.len());
    Ok(result_from_file(file, None))
}

async fn rename_file_action(
    ctx: &ToolContext,
    target: &Path,
    options: &FileActionOptions,
) -> Result<ToolResult> {
    let before = read_existing_with_hash_check(target, options)?;
    let new_path = options
        .new_file_path
        .as_ref()
        .ok_or_else(|| anyhow!("newFilePath is required for mode=rename"))?;
    let new_path = ctx.resolve(new_path);
    if new_path.exists() {
        return Err(anyhow!(
            "cannot rename over existing file: {}",
            new_path.display()
        ));
    }
    fs::rename(target, &new_path).with_context(|| {
        format!(
            "failed to rename file {} to {}",
            target.display(),
            new_path.display()
        )
    })?;
    let file = rename_file(ctx, target, &new_path);
    Ok(result_from_file(
        file,
        options.hash_check_mode.then_some(hash_bytes(&before)),
    ))
}

async fn apply_text_patch(
    ctx: &ToolContext,
    target: &Path,
    before_bytes: Vec<u8>,
    patch_text: &str,
    options: &FileActionOptions,
) -> Result<ToolResult> {
    let shape = TextShape::from_bytes(before_bytes)?;
    let after_text = apply_line_text_patch(&shape.text, patch_text)?;
    let after_bytes = shape.encode(&after_text);
    fs::write(target, &after_bytes)
        .with_context(|| format!("failed to write patch target {}", target.display()))?;

    let after_hash = hash_bytes(&after_bytes);
    let diff = diff_text(target, &shape.text, &after_text);
    let file = patch_file(ctx, target, &diff);
    Ok(result_from_file_with_diff(
        file,
        options.hash_check_mode.then_some(after_hash),
        Some(diff),
    ))
}

async fn apply_binary_patch(
    ctx: &ToolContext,
    target: &Path,
    before_bytes: Vec<u8>,
    patch_text: &str,
    options: &FileActionOptions,
) -> Result<ToolResult> {
    let hunks = parse_binary_patch(patch_text)?;
    let applied = apply_binary_hunks(&before_bytes, &hunks)?;
    let after_bytes = applied.bytes;
    fs::write(target, &after_bytes)
        .with_context(|| format!("failed to write patch target {}", target.display()))?;

    let after_hash = hash_bytes(&after_bytes);
    let file = binary_patch_file(ctx, target, applied.additions, applied.deletions);
    let diff = diff_binary(target, &before_bytes, &after_bytes);
    Ok(result_from_file_with_diff(
        file,
        options.hash_check_mode.then_some(after_hash),
        Some(diff),
    ))
}

impl TextShape {
    fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        if let Some((offset, byte)) = binary_byte(&bytes) {
            return Err(anyhow!(
                "Cannot patch binary file (binary byte at offset {}: 0x{:02X})",
                offset,
                byte
            ));
        }
        let raw = String::from_utf8(bytes).context("Cannot patch non-UTF-8 text file")?;
        let line_ending = detect_line_ending(&raw);
        let (bom, raw) = raw
            .strip_prefix('\u{FEFF}')
            .map(|text| (true, text.to_string()))
            .unwrap_or((false, raw));
        Ok(Self {
            bom,
            line_ending,
            text: normalize_to_lf(&raw),
        })
    }

    fn encode(&self, text: &str) -> Vec<u8> {
        let mut raw = restore_line_endings(text, self.line_ending);
        if self.bom {
            raw.insert(0, '\u{FEFF}');
        }
        raw.into_bytes()
    }
}

fn result_from_file(file: PatchFile, hash_code: Option<String>) -> ToolResult {
    result_from_file_with_diff(file, hash_code, None)
}

fn result_from_file_with_diff(
    file: PatchFile,
    hash_code: Option<String>,
    diff: Option<String>,
) -> ToolResult {
    let mut output = match file.kind.as_str() {
        "create" => format!("Success. Created file:\nC {}", file.relative_path),
        "delete" => format!("Success. Deleted file:\nD {}", file.relative_path),
        "rename" => format!(
            "Success. Renamed file:\nR {} -> {}",
            file.relative_path,
            file.new_relative_path.as_deref().unwrap_or_default()
        ),
        _ => format!("Success. Updated file:\nM {}", file.relative_path),
    };
    if let Some(hash_code) = &hash_code {
        output.push_str(&format!("\nhashCode: {hash_code}"));
    }

    let mut metadata = json!({ "file": file, "diagnostics": {} });
    if let Some(hash_code) = hash_code {
        metadata["hashCode"] = Value::String(hash_code);
    }
    if let Some(diff) = diff.filter(|value| !value.trim().is_empty()) {
        metadata["diff"] = Value::String(diff);
    }

    ToolResult {
        metadata,
        output: tool_output(output),
    }
}

fn read_existing_with_hash_check(path: &Path, options: &FileActionOptions) -> Result<Vec<u8>> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read target file {}", path.display()))?;
    if options.hash_check_mode {
        let expected = normalize_hash_code(options.hash_code.as_deref().unwrap_or_default())?;
        let current = hash_bytes(&bytes);
        if expected != current {
            return Err(anyhow!(
                "hash mismatch for {}: expected {}, current {}; re-read and retry",
                path.display(),
                expected,
                current
            ));
        }
    }
    Ok(bytes)
}

fn apply_line_text_patch(before_text: &str, patch_text: &str) -> Result<String> {
    if patch_text
        .lines()
        .any(|line| line.trim() == "*** Begin Patch")
    {
        return Err(anyhow!("old patch envelope format is not supported"));
    }
    let ops = parse_line_patch(patch_text)?;
    if ops.is_empty() {
        return Err(anyhow!(
            "patchText did not contain any line patch instructions"
        ));
    }
    apply_line_patch(before_text, &ops)
}

fn parse_line_patch(patch_text: &str) -> Result<Vec<LinePatchOp>> {
    let lines: Vec<&str> = patch_text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let mut ops = Vec::new();
    let mut idx = 0usize;
    while idx < lines.len() {
        let line = lines[idx];
        let trimmed = line.trim();
        idx += 1;
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("***DELETE***") {
            let (start, end) = parse_line_range(rest.trim())
                .with_context(|| format!("invalid DELETE instruction: `{line}`"))?;
            ops.push(LinePatchOp::Delete { start, end });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("***MOVE***") {
            let (range, after) = rest
                .trim()
                .split_once(',')
                .ok_or_else(|| anyhow!("MOVE must be `***MOVE*** start-end,startline`"))?;
            let (start, end) = parse_line_range(range.trim())
                .with_context(|| format!("invalid MOVE range: `{line}`"))?;
            let after = parse_line_insert_target(after.trim())
                .with_context(|| format!("invalid MOVE target: `{line}`"))?;
            ops.push(LinePatchOp::Move { start, end, after });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("***APPEND_HEAD***") {
            let after = parse_line_insert_target(rest.trim())
                .with_context(|| format!("invalid APPEND_HEAD target: `{line}`"))?;
            let mut block = Vec::new();
            let mut found_end = false;
            while idx < lines.len() {
                let item = lines[idx];
                idx += 1;
                if item.trim() == "***APPEND_END***" {
                    found_end = true;
                    break;
                }
                block.push(item.to_string());
            }
            if !found_end {
                return Err(anyhow!("APPEND_HEAD block is missing ***APPEND_END***"));
            }
            ops.push(LinePatchOp::Append {
                after,
                lines: block,
            });
            continue;
        }
        if trimmed == "***APPEND_END***" {
            return Err(anyhow!("APPEND_END without APPEND_HEAD"));
        }
        if let Some((number, text)) = line.split_once(':') {
            let line_number = parse_line_number(number.trim(), false)
                .with_context(|| format!("invalid replace instruction: `{line}`"))?;
            ops.push(LinePatchOp::Replace {
                line: line_number,
                text: text.to_string(),
            });
            continue;
        }
        return Err(anyhow!(
            "invalid patchText instruction `{line}`; expected `***DELETE*** start-end`, `***MOVE*** start-end,startline`, `***APPEND_HEAD*** startline ... ***APPEND_END***`, or `n:new text`"
        ));
    }
    Ok(ops)
}

fn parse_line_range(value: &str) -> Result<(usize, usize)> {
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| anyhow!("line range must be `start-end`"))?;
    let start = parse_line_number(start.trim(), false)?;
    let end = parse_line_number(end.trim(), false)?;
    if start > end {
        return Err(anyhow!("line range start {start} is after end {end}"));
    }
    Ok((start, end))
}

fn parse_line_number(value: &str, allow_zero: bool) -> Result<usize> {
    let number = value
        .parse::<usize>()
        .with_context(|| format!("expected a positive line number, got `{value}`"))?;
    if number == 0 && !allow_zero {
        return Err(anyhow!("line number must be >= 1"));
    }
    Ok(number)
}

fn parse_line_insert_target(value: &str) -> Result<LineInsertTarget> {
    if value == "-1" {
        return Ok(LineInsertTarget::End);
    }
    let number = parse_line_number(value, true)?;
    if number == 0 {
        Ok(LineInsertTarget::Start)
    } else {
        Ok(LineInsertTarget::After(number))
    }
}

fn apply_line_patch(before_text: &str, ops: &[LinePatchOp]) -> Result<String> {
    let had_final_newline = before_text.ends_with('\n');
    let lines: Vec<String> = if before_text.is_empty() {
        Vec::new()
    } else {
        before_text
            .split_terminator('\n')
            .map(str::to_string)
            .collect()
    };
    let line_count = lines.len();
    let mut removals = vec![None; line_count];
    let mut replacements = vec![None; line_count];
    let mut pending_insertions = Vec::new();

    for (order, op) in ops.iter().enumerate() {
        match op {
            LinePatchOp::Delete { start, end } => {
                ensure_line_range(&lines, *start, *end, "DELETE")?;
                mark_line_removal(&mut removals, *start, *end, LineRemoval::Delete)?;
            }
            LinePatchOp::Move { start, end, after } => {
                ensure_line_range(&lines, *start, *end, "MOVE")?;
                if matches!(after, LineInsertTarget::After(line) if (*start..=*end).contains(line))
                {
                    return Err(anyhow!(
                        "MOVE target line {} is inside moved range {start}-{end}",
                        after.label()
                    ));
                }
                let boundary = after.index(line_count, "MOVE target")?;
                mark_line_removal(&mut removals, *start, *end, LineRemoval::Move)?;
                pending_insertions.push(PendingLineInsertion {
                    boundary,
                    order,
                    kind: PendingLineInsertionKind::Move {
                        start: *start,
                        end: *end,
                    },
                });
            }
            LinePatchOp::Append {
                after,
                lines: block,
            } => {
                let boundary = after.index(line_count, "APPEND_HEAD target")?;
                pending_insertions.push(PendingLineInsertion {
                    boundary,
                    order,
                    kind: PendingLineInsertionKind::Literal(block.clone()),
                });
            }
            LinePatchOp::Replace { line, text } => {
                ensure_line_range(&lines, *line, *line, "replace")?;
                let replacement = &mut replacements[line - 1];
                if replacement.is_some() {
                    return Err(anyhow!("replace line {line} is targeted more than once"));
                }
                *replacement = Some(text.clone());
            }
        }
    }

    for (idx, replacement) in replacements.iter().enumerate() {
        if replacement.is_some() && removals[idx] == Some(LineRemoval::Delete) {
            return Err(anyhow!("replace line {} overlaps deleted line", idx + 1));
        }
    }

    let insertions = resolve_line_insertions(&lines, &replacements, pending_insertions);
    let mut output_lines = Vec::new();
    push_line_insertions(&mut output_lines, &insertions[0]);
    for line_idx in 0..line_count {
        if removals[line_idx].is_none() {
            output_lines.push(line_text(&lines, &replacements, line_idx + 1));
        }
        push_line_insertions(&mut output_lines, &insertions[line_idx + 1]);
    }

    let mut out = output_lines.join("\n");
    if had_final_newline || !output_lines.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

fn mark_line_removal(
    removals: &mut [Option<LineRemoval>],
    start: usize,
    end: usize,
    removal: LineRemoval,
) -> Result<()> {
    for line in start..=end {
        if let Some(existing) = removals[line - 1] {
            return Err(anyhow!(
                "{} line range {start}-{end} overlaps {} line {line}",
                removal.label(),
                existing.label()
            ));
        }
    }
    for line in start..=end {
        removals[line - 1] = Some(removal);
    }
    Ok(())
}

fn resolve_line_insertions(
    lines: &[String],
    replacements: &[Option<String>],
    pending_insertions: Vec<PendingLineInsertion>,
) -> Vec<Vec<LineInsertion>> {
    let mut insertions: Vec<Vec<LineInsertion>> = (0..=lines.len()).map(|_| Vec::new()).collect();
    for pending in pending_insertions {
        let lines = match pending.kind {
            PendingLineInsertionKind::Literal(lines) => lines,
            PendingLineInsertionKind::Move { start, end } => (start..=end)
                .map(|line| line_text(lines, replacements, line))
                .collect(),
        };
        insertions[pending.boundary].push(LineInsertion {
            order: pending.order,
            lines,
        });
    }
    for boundary in &mut insertions {
        boundary.sort_by_key(|insertion| insertion.order);
    }
    insertions
}

fn push_line_insertions(output_lines: &mut Vec<String>, insertions: &[LineInsertion]) {
    for insertion in insertions {
        output_lines.extend(insertion.lines.iter().cloned());
    }
}

fn line_text(lines: &[String], replacements: &[Option<String>], line: usize) -> String {
    replacements[line - 1]
        .as_ref()
        .cloned()
        .unwrap_or_else(|| lines[line - 1].clone())
}

impl LineInsertTarget {
    fn index(self, line_count: usize, label: &str) -> Result<usize> {
        match self {
            LineInsertTarget::Start => Ok(0),
            LineInsertTarget::End => Ok(line_count),
            LineInsertTarget::After(line) => {
                if line > line_count {
                    return Err(anyhow!(
                        "{label} line {line} is out of range for {line_count} line(s)"
                    ));
                }
                Ok(line)
            }
        }
    }

    fn label(self) -> String {
        match self {
            LineInsertTarget::Start => "0".to_string(),
            LineInsertTarget::End => "-1".to_string(),
            LineInsertTarget::After(line) => line.to_string(),
        }
    }
}

impl LineRemoval {
    fn label(self) -> &'static str {
        match self {
            LineRemoval::Delete => "DELETE",
            LineRemoval::Move => "MOVE",
        }
    }
}

fn ensure_line_range(lines: &[String], start: usize, end: usize, label: &str) -> Result<()> {
    if start == 0 || end == 0 || start > end || end > lines.len() {
        return Err(anyhow!(
            "{label} line range {start}-{end} is out of range for {} line(s)",
            lines.len()
        ));
    }
    Ok(())
}

fn parse_binary_patch(patch_text: &str) -> Result<Vec<BinaryHunk>> {
    if patch_text
        .lines()
        .any(|line| line.trim() == "*** Begin Patch")
    {
        return Err(anyhow!(
            "old patch envelope format is not supported; pass filePath separately and use binary patchText"
        ));
    }

    let mut hunks = Vec::new();
    for (order, raw) in patch_text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("***DELETE***") {
            let (offset, len) = parse_binary_offset_len(rest.trim())?;
            hunks.push(BinaryHunk {
                anchor: BinaryAnchor::Delete { offset, len },
                bytes: Vec::new(),
                order,
            });
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("***APPEND***") {
            let (span, hex) = split_binary_body(rest.trim(), line)?;
            let (target, len) = parse_binary_append_span(span)?;
            let bytes = decode_hex(hex)?;
            ensure_binary_body_len(bytes.len(), len, line)?;
            hunks.push(BinaryHunk {
                anchor: BinaryAnchor::Append { target, len },
                bytes,
                order,
            });
            continue;
        }

        if let Some((span, hex)) = trimmed.split_once(':') {
            let (offset, len) = parse_binary_offset_len(span.trim())?;
            let bytes = decode_hex(hex)?;
            ensure_binary_body_len(bytes.len(), len, line)?;
            hunks.push(BinaryHunk {
                anchor: BinaryAnchor::Replace { offset, len },
                bytes,
                order,
            });
            continue;
        }

        return Err(anyhow!(
            "invalid binary patch instruction `{line}`; expected `***DELETE*** offset-len`, `***APPEND*** offset-len:HEX`, or `offset-len:HEX`"
        ));
    }
    if hunks.is_empty() {
        return Err(anyhow!("patchText did not contain any hunks"));
    }
    Ok(hunks)
}

fn parse_byte_offset(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .with_context(|| format!("invalid byte offset `{value}`"))
}

fn parse_byte_len(value: &str) -> Result<usize> {
    let len = value
        .parse::<usize>()
        .with_context(|| format!("invalid byte length `{value}`"))?;
    if len == 0 {
        return Err(anyhow!("byte length must be greater than 0"));
    }
    Ok(len)
}

fn parse_binary_offset_len(value: &str) -> Result<(usize, usize)> {
    let (offset, len) = value
        .rsplit_once('-')
        .ok_or_else(|| anyhow!("binary span must be `offset-len`"))?;
    Ok((
        parse_byte_offset(offset.trim())?,
        parse_byte_len(len.trim())?,
    ))
}

fn parse_binary_append_span(value: &str) -> Result<(BinaryAppendTarget, usize)> {
    let (offset, len) = value
        .rsplit_once('-')
        .ok_or_else(|| anyhow!("binary append span must be `offset-len`"))?;
    let target = if offset.trim() == "-1" {
        BinaryAppendTarget::End
    } else {
        BinaryAppendTarget::Offset(parse_byte_offset(offset.trim())?)
    };
    Ok((target, parse_byte_len(len.trim())?))
}

fn split_binary_body<'a>(value: &'a str, line: &str) -> Result<(&'a str, &'a str)> {
    value
        .split_once(':')
        .map(|(span, body)| (span.trim(), body))
        .ok_or_else(|| anyhow!("binary patch instruction `{line}` is missing `:HEX`"))
}

fn ensure_binary_body_len(actual: usize, expected: usize, line: &str) -> Result<()> {
    if actual != expected {
        return Err(anyhow!(
            "binary patch instruction `{line}` declares {expected} byte(s) but contains {actual} byte(s)"
        ));
    }
    Ok(())
}

fn apply_binary_hunks(bytes: &[u8], hunks: &[BinaryHunk]) -> Result<BinaryPatchApplied> {
    let mut ops = hunks
        .iter()
        .map(|hunk| hunk_to_binary_operation(hunk, bytes.len()))
        .collect::<Result<Vec<_>>>()?;
    ops.sort_by_key(|op| (op.start, op.end > op.start, op.order));

    let mut output = Vec::new();
    let mut cursor = 0usize;
    let additions = ops.iter().map(|op| op.additions).sum();
    let deletions = ops.iter().map(|op| op.deletions).sum();
    for op in ops {
        if op.start < cursor {
            return Err(anyhow!(
                "binary patch hunks overlap or target already replaced bytes"
            ));
        }
        output.extend_from_slice(&bytes[cursor..op.start]);
        output.extend(op.replacement);
        cursor = op.end;
    }
    output.extend_from_slice(&bytes[cursor..]);
    Ok(BinaryPatchApplied {
        bytes: output,
        additions,
        deletions,
    })
}

fn hunk_to_binary_operation(hunk: &BinaryHunk, total: usize) -> Result<BinaryOperation> {
    let (start, end, additions, deletions) = match hunk.anchor {
        BinaryAnchor::Replace { offset, len } => {
            let end = byte_range_end(offset, len, total)?;
            (offset, end, len, len)
        }
        BinaryAnchor::Delete { offset, len } => {
            let end = byte_range_end(offset, len, total)?;
            (offset, end, 0, len)
        }
        BinaryAnchor::Append {
            target: BinaryAppendTarget::Offset(offset),
            len,
        } => {
            ensure_append_offset(offset, total)?;
            (offset, offset, len, 0)
        }
        BinaryAnchor::Append {
            target: BinaryAppendTarget::End,
            len,
        } => (total, total, len, 0),
    };

    Ok(BinaryOperation {
        start,
        end,
        replacement: hunk.bytes.clone(),
        order: hunk.order,
        additions,
        deletions,
    })
}

fn byte_range_end(offset: usize, len: usize, total: usize) -> Result<usize> {
    offset
        .checked_add(len)
        .filter(|end| *end <= total)
        .ok_or_else(|| {
            anyhow!(
                "byte range {}..{} is out of range for this file ({total} bytes)",
                offset,
                offset.saturating_add(len)
            )
        })
}

fn ensure_append_offset(offset: usize, total: usize) -> Result<()> {
    if offset > total {
        return Err(anyhow!(
            "append offset {offset} is out of range for this file ({total} bytes); use offset 0 for the start or -1 for the end"
        ));
    }
    Ok(())
}

fn normalize_hash_code(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let digest = trimmed.strip_prefix("sha256:").unwrap_or(trimmed);
    if digest.len() != 64 || !digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "hashCode must be a full SHA-256 digest, optionally prefixed with sha256:"
        ));
    }
    Ok(format!("sha256:{}", digest.to_ascii_lowercase()))
}

fn detect_line_ending(text: &str) -> &'static str {
    let crlf = text.find("\r\n");
    let lf = text.find('\n');
    match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf <= lf => "\r\n",
        (Some(_), None) => "\r\n",
        _ => "\n",
    }
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn restore_line_endings(text: &str, line_ending: &str) -> String {
    if line_ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

fn patch_file(ctx: &ToolContext, path: &Path, diff: &str) -> PatchFile {
    let additions = count_diff_lines(diff, '+');
    let deletions = count_diff_lines(diff, '-');
    PatchFile {
        file_path: path.to_string_lossy().into_owned(),
        relative_path: ctx.title(path),
        kind: "update".to_string(),
        new_file_path: None,
        new_relative_path: None,
        additions,
        deletions,
    }
}

fn action_file(
    ctx: &ToolContext,
    path: &Path,
    kind: &str,
    additions: usize,
    deletions: usize,
) -> PatchFile {
    PatchFile {
        file_path: path.to_string_lossy().into_owned(),
        relative_path: ctx.title(path),
        new_file_path: None,
        new_relative_path: None,
        kind: kind.to_string(),
        additions,
        deletions,
    }
}

fn rename_file(ctx: &ToolContext, from: &Path, to: &Path) -> PatchFile {
    PatchFile {
        file_path: from.to_string_lossy().into_owned(),
        relative_path: ctx.title(from),
        new_file_path: Some(to.to_string_lossy().into_owned()),
        new_relative_path: Some(ctx.title(to)),
        kind: "rename".to_string(),
        additions: 0,
        deletions: 0,
    }
}
fn diff_text(path: &Path, before: &str, after: &str) -> String {
    let diff = diffy_create_patch(before, after).to_string();
    diff.replacen("--- original", &format!("--- {}", path.display()), 1)
        .replacen("+++ modified", &format!("+++ {}", path.display()), 1)
}

fn diff_binary(path: &Path, before: &[u8], after: &[u8]) -> String {
    diff_text(path, &binary_diff_text(before), &binary_diff_text(after))
}

fn binary_diff_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "(empty)\n".to_string();
    }
    let mut lines = bytes
        .chunks(16)
        .enumerate()
        .map(|(idx, chunk)| {
            let hex = chunk
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{:08X}: {hex}", idx * 16)
        })
        .collect::<Vec<_>>()
        .join("\n");
    lines.push('\n');
    lines
}

fn count_diff_lines(diff: &str, marker: char) -> usize {
    diff.lines()
        .filter(|line| line.starts_with(marker))
        .filter(|line| !line.starts_with("+++") && !line.starts_with("---"))
        .count()
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let compact = text
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.strip_prefix("0x")
                .or_else(|| part.strip_prefix("0X"))
                .unwrap_or(part)
        })
        .collect::<Vec<_>>()
        .join("");
    if compact.len() % 2 != 0 {
        return Err(anyhow!(
            "hex byte content must contain an even number of digits"
        ));
    }
    (0..compact.len())
        .step_by(2)
        .map(|idx| {
            u8::from_str_radix(&compact[idx..idx + 2], 16)
                .map_err(|err| anyhow!("invalid hex byte at digit {idx}: {err}"))
        })
        .collect()
}

fn binary_byte(bytes: &[u8]) -> Option<(usize, u8)> {
    bytes
        .iter()
        .take(4096)
        .enumerate()
        .find_map(|(idx, byte)| (*byte == 0).then_some((idx, *byte)))
}
