mod binary_patch;
mod options;
mod result;
#[cfg(test)]
mod test;
mod text_patch;

use crate::{hash_bytes, tool_output, ToolContext, ToolResult};
use anyhow::{anyhow, Context, Result};
use diffy::{apply as diffy_apply, create_patch as diffy_create_patch, Patch as DiffyPatch};
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

fn binary_patch_file(ctx: &ToolContext, path: &Path, before: &[u8], after: &[u8]) -> PatchFile {
    PatchFile {
        file_path: path.to_string_lossy().into_owned(),
        relative_path: ctx.title(path),
        new_file_path: None,
        new_relative_path: None,
        kind: "binary-update".to_string(),
        additions: after.len(),
        deletions: before.len(),
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
    Replace { offset: usize, len: ByteLen },
    Delete { offset: usize, len: ByteLen },
    Insert { target: BinaryInsertTarget },
}

#[derive(Clone, Copy, Debug)]
enum ByteLen {
    Count(usize),
    Rest,
}

#[derive(Clone, Debug)]
enum BinaryInsertTarget {
    Start,
    Offset(usize),
    End,
}

#[derive(Clone, Debug)]
struct BinaryOperation {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
    order: usize,
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
    let after_text = apply_diffy_text_patch(&shape.text, patch_text)?;
    let after_bytes = shape.encode(&after_text);
    fs::write(target, &after_bytes)
        .with_context(|| format!("failed to write patch target {}", target.display()))?;

    let after_hash = hash_bytes(&after_bytes);
    let file = patch_file(ctx, target, &shape.text, &after_text);
    Ok(result_from_file(
        file,
        options.hash_check_mode.then_some(after_hash),
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
    let after_bytes = apply_binary_hunks(&before_bytes, &hunks)?;
    fs::write(target, &after_bytes)
        .with_context(|| format!("failed to write patch target {}", target.display()))?;

    let after_hash = hash_bytes(&after_bytes);
    let file = binary_patch_file(ctx, target, &before_bytes, &after_bytes);
    Ok(result_from_file(
        file,
        options.hash_check_mode.then_some(after_hash),
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

fn apply_diffy_text_patch(before_text: &str, patch_text: &str) -> Result<String> {
    let trimmed = patch_text.trim_start();
    if patch_text
        .lines()
        .any(|line| line.trim() == "*** Begin Patch")
    {
        return Err(anyhow!("old patch envelope format is not supported"));
    }
    let owned_patch;
    let patch_source = if trimmed.starts_with("@@") {
        owned_patch = format!("--- file\n+++ file\n{patch_text}");
        owned_patch.as_str()
    } else {
        patch_text
    };
    let patch = DiffyPatch::from_str(patch_source).context("failed to parse unified diff patch")?;
    if patch.hunks().is_empty() {
        return Err(anyhow!("patchText must contain at least one unified diff hunk"));
    }
    diffy_apply(before_text, &patch).context("failed to apply unified diff patch")
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
    let mut current: Option<BinaryHunk> = None;
    for raw in patch_text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if let Some(anchor) = parse_binary_anchor(line)? {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(BinaryHunk {
                anchor,
                bytes: Vec::new(),
                order: hunks.len(),
            });
            continue;
        }

        let Some(hunk) = current.as_mut() else {
            return Err(anyhow!(
                "binary patchText must start with a hunk header such as `replace 0 1`, `delete 0 1`, `insert 0`, or `insert -1`"
            ));
        };
        if let Some(hex) = line.strip_prefix('+') {
            hunk.bytes.extend(decode_hex(hex)?);
        } else if line.starts_with("copy ") {
            return Err(anyhow!(
                "copy body lines are not supported in binary patch mode"
            ));
        } else {
            return Err(anyhow!(
                "unsupported binary patch body line `{line}`; body lines must start with `+`"
            ));
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    if hunks.is_empty() {
        return Err(anyhow!("patchText did not contain any hunks"));
    }
    for hunk in &hunks {
        match hunk.anchor {
            BinaryAnchor::Delete { .. } if !hunk.bytes.is_empty() => {
                return Err(anyhow!("delete hunks cannot contain body lines"));
            }
            BinaryAnchor::Delete { .. } => {}
            _ if hunk.bytes.is_empty() => {
                return Err(anyhow!("non-delete binary hunks require at least one byte"));
            }
            _ => {}
        }
    }
    Ok(hunks)
}

fn parse_binary_anchor(line: &str) -> Result<Option<BinaryAnchor>> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["insert", offset] => Ok(Some(BinaryAnchor::Insert {
            target: parse_binary_insert_target(offset)?,
        })),
        ["replace", offset, len] => Ok(Some(BinaryAnchor::Replace {
            offset: parse_byte_offset(offset)?,
            len: parse_byte_len(len)?,
        })),
        ["delete", offset, len] => Ok(Some(BinaryAnchor::Delete {
            offset: parse_byte_offset(offset)?,
            len: parse_byte_len(len)?,
        })),
        _ => Ok(None),
    }
}

fn parse_binary_insert_target(value: &str) -> Result<BinaryInsertTarget> {
    if value == "0" {
        return Ok(BinaryInsertTarget::Start);
    }
    if value == "-1" {
        return Ok(BinaryInsertTarget::End);
    }
    Ok(BinaryInsertTarget::Offset(parse_byte_offset(value)?))
}

fn parse_byte_offset(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .with_context(|| format!("invalid byte offset `{value}`"))
}

fn parse_byte_len(value: &str) -> Result<ByteLen> {
    if value == "-1" {
        return Ok(ByteLen::Rest);
    }
    let len = value
        .parse::<usize>()
        .with_context(|| format!("invalid byte length `{value}`"))?;
    if len == 0 {
        return Err(anyhow!("byte length must be greater than 0"));
    }
    Ok(ByteLen::Count(len))
}

fn apply_binary_hunks(bytes: &[u8], hunks: &[BinaryHunk]) -> Result<Vec<u8>> {
    let mut ops = hunks
        .iter()
        .map(|hunk| hunk_to_binary_operation(hunk, bytes.len()))
        .collect::<Result<Vec<_>>>()?;
    ops.sort_by_key(|op| (op.start, op.end > op.start, op.order));

    let mut output = Vec::new();
    let mut cursor = 0usize;
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
    Ok(output)
}

fn hunk_to_binary_operation(hunk: &BinaryHunk, total: usize) -> Result<BinaryOperation> {
    let (start, end) = match hunk.anchor {
        BinaryAnchor::Replace { offset, len } | BinaryAnchor::Delete { offset, len } => {
            let end = byte_range_end(offset, len, total)?;
            (offset, end)
        }
        BinaryAnchor::Insert {
            target: BinaryInsertTarget::Start,
        } => (0, 0),
        BinaryAnchor::Insert {
            target: BinaryInsertTarget::Offset(offset),
        } => {
            ensure_insert_offset(offset, total)?;
            (offset, offset)
        }
        BinaryAnchor::Insert {
            target: BinaryInsertTarget::End,
        } => (total, total),
    };

    Ok(BinaryOperation {
        start,
        end,
        replacement: hunk.bytes.clone(),
        order: hunk.order,
    })
}

fn byte_range_end(offset: usize, len: ByteLen, total: usize) -> Result<usize> {
    if offset >= total {
        return Err(anyhow!(
            "byte offset {offset} is out of range for this file ({total} bytes)"
        ));
    }
    match len {
        ByteLen::Count(len) => offset
            .checked_add(len)
            .filter(|end| *end <= total)
            .ok_or_else(|| {
                anyhow!(
                    "byte range {}..{} is out of range for this file ({total} bytes)",
                    offset,
                    offset.saturating_add(len)
                )
            }),
        ByteLen::Rest => Ok(total),
    }
}

fn ensure_insert_offset(offset: usize, total: usize) -> Result<()> {
    if offset >= total {
        return Err(anyhow!(
            "insert offset {offset} is out of range for this file ({total} bytes); use insert 0 for the start or insert -1 for the end"
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

fn patch_file(ctx: &ToolContext, path: &Path, before: &str, after: &str) -> PatchFile {
    let diff = diff_text(path, before, after);
    let additions = count_diff_lines(&diff, '+');
    let deletions = count_diff_lines(&diff, '-');
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
