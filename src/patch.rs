use crate::fs_ops::hash_bytes;
use crate::{ToolContext, ToolResult};
use anyhow::{anyhow, Context, Result};
use diffy::create_patch as diffy_create_patch;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
pub struct ApplyOptions {
    #[serde(rename = "filePath")]
    pub file_path: PathBuf,
    #[serde(rename = "patchText")]
    pub patch_text: String,
    #[serde(default, rename = "patchMode")]
    pub patch_mode: PatchMode,
    #[serde(default, rename = "hashCheckMode")]
    pub hash_check_mode: bool,
    #[serde(default, rename = "hashCode")]
    pub hash_code: Option<String>,
}

fn binary_patch_file(ctx: &ToolContext, path: &Path, before: &[u8], after: &[u8]) -> PatchFile {
    let before_hex = bytes_hex(before);
    let after_hex = bytes_hex(after);
    let diff = format!(
        "Binary update: {}\n- {} bytes: {}\n+ {} bytes: {}\n",
        path.display(),
        before.len(),
        before_hex,
        after.len(),
        after_hex
    );
    PatchFile {
        file_path: path.to_string_lossy().into_owned(),
        relative_path: ctx.title(path),
        kind: "binary-update".to_string(),
        diff,
        before: before_hex,
        after: after_hex,
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
    #[serde(rename = "type")]
    pub kind: String,
    pub diff: String,
    pub before: String,
    pub after: String,
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
struct Hunk {
    anchor: Anchor,
    body: Vec<BodyLine>,
    order: usize,
}

#[derive(Clone, Debug)]
enum Anchor {
    Replace { start: usize, end: usize },
    Delete { start: usize, end: usize },
    Insert { target: InsertTarget },
}

#[derive(Clone, Debug)]
enum InsertTarget {
    Start,
    After(usize),
    End,
}

#[derive(Clone, Debug)]
enum BodyLine {
    Literal(String),
    Copy { start: usize, end: usize },
}

#[derive(Clone, Debug)]
struct Operation {
    start: usize,
    end: usize,
    replacement: Vec<String>,
    order: usize,
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

pub async fn apply_patch(options: ApplyOptions, ctx: &ToolContext) -> Result<ToolResult> {
    if options.patch_text.trim().is_empty() {
        return Err(anyhow!("patchText is required"));
    }
    if options.hash_check_mode && options.hash_code.as_deref().unwrap_or("").trim().is_empty() {
        return Err(anyhow!("hashCode is required when hashCheckMode is true"));
    }

    let target = ctx.resolve(&options.file_path);
    let before_bytes = fs::read(&target)
        .with_context(|| format!("failed to read patch target {}", target.display()))?;
    let before_hash = hash_bytes(&before_bytes);
    if options.hash_check_mode {
        let expected = normalize_hash_code(options.hash_code.as_deref().unwrap_or_default())?;
        if expected != before_hash {
            return Err(anyhow!(
                "hash mismatch for {}: expected {}, current {}; re-read and retry",
                target.display(),
                expected,
                before_hash
            ));
        }
    }

    match options.patch_mode {
        PatchMode::Text => apply_text_patch(ctx, &target, before_bytes, &options).await,
        PatchMode::Binary => apply_binary_patch(ctx, &target, before_bytes, &options).await,
    }
}

async fn apply_text_patch(
    ctx: &ToolContext,
    target: &Path,
    before_bytes: Vec<u8>,
    options: &ApplyOptions,
) -> Result<ToolResult> {
    let shape = TextShape::from_bytes(before_bytes)?;
    let hunks = parse_line_patch(&options.patch_text)?;
    let after_text = apply_hunks(&shape.text, &hunks)?;
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
    options: &ApplyOptions,
) -> Result<ToolResult> {
    let hunks = parse_binary_patch(&options.patch_text)?;
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
    let mut output = format!("Success. Updated file:\nM {}", file.relative_path);
    if let Some(hash_code) = &hash_code {
        output.push_str(&format!("\nhashCode: {hash_code}"));
    }

    let mut metadata = json!({ "diff": file.diff, "file": file, "diagnostics": {} });
    if let Some(hash_code) = hash_code {
        metadata["hashCode"] = Value::String(hash_code);
    }

    ToolResult {
        title: output.clone(),
        metadata,
        output,
    }
}

fn parse_line_patch(patch_text: &str) -> Result<Vec<Hunk>> {
    if patch_text
        .lines()
        .any(|line| line.trim() == "*** Begin Patch")
    {
        return Err(anyhow!(
            "old apply_patch envelope format is not supported; pass filePath separately and use line-number patchText"
        ));
    }

    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    for raw in patch_text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if let Some(anchor) = parse_anchor(line)? {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(Hunk {
                anchor,
                body: Vec::new(),
                order: hunks.len(),
            });
            continue;
        }

        let Some(hunk) = current.as_mut() else {
            return Err(anyhow!(
                "patchText must start with a hunk header such as `replace 1 1`, `delete 1 1`, `insert 1`, or `insert -1`"
            ));
        };
        if let Some(text) = line.strip_prefix('+') {
            hunk.body.push(BodyLine::Literal(text.to_string()));
        } else if let Some(range) = line.strip_prefix("copy ") {
            let (start, end) = parse_copy_range(range.trim())?;
            hunk.body.push(BodyLine::Copy { start, end });
        } else {
            return Err(anyhow!(
                "unsupported patch body line `{line}`; body lines must start with `+` or `copy `"
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
            Anchor::Delete { .. } if !hunk.body.is_empty() => {
                return Err(anyhow!("delete hunks cannot contain body lines"));
            }
            Anchor::Delete { .. } => {}
            _ if hunk.body.is_empty() => {
                return Err(anyhow!("non-delete hunks require at least one body line"));
            }
            _ => {}
        }
    }
    Ok(hunks)
}

fn parse_anchor(line: &str) -> Result<Option<Anchor>> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["insert", line] => Ok(Some(Anchor::Insert {
            target: parse_insert_target(line)?,
        })),
        ["replace", start, end] => Ok(Some(Anchor::Replace {
            start: parse_line_number(start)?,
            end: parse_line_number(end)?,
        })),
        ["delete", start, end] => Ok(Some(Anchor::Delete {
            start: parse_line_number(start)?,
            end: parse_line_number(end)?,
        })),
        _ => Ok(None),
    }
}

fn parse_copy_range(value: &str) -> Result<(usize, usize)> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    let [start, end] = parts.as_slice() else {
        return Err(anyhow!("copy body lines must be `copy A B`"));
    };
    let start = parse_line_number(start.trim())?;
    let end = parse_line_number(end.trim())?;
    if start > end {
        return Err(anyhow!("copy range start must be <= end: copy {value}"));
    }
    Ok((start, end))
}

fn parse_line_number(value: &str) -> Result<usize> {
    let line = value
        .parse::<usize>()
        .with_context(|| format!("invalid line number `{value}`"))?;
    if line == 0 {
        return Err(anyhow!("line numbers are 1-based"));
    }
    Ok(line)
}

fn parse_insert_target(value: &str) -> Result<InsertTarget> {
    if value == "0" {
        return Ok(InsertTarget::Start);
    }
    if value == "-1" {
        return Ok(InsertTarget::End);
    }
    Ok(InsertTarget::After(parse_line_number(value)?))
}

fn apply_hunks(text: &str, hunks: &[Hunk]) -> Result<String> {
    let (lines, final_newline) = split_text_lines(text);
    let mut ops = hunks
        .iter()
        .map(|hunk| hunk_to_operation(hunk, &lines))
        .collect::<Result<Vec<_>>>()?;
    ops.sort_by_key(|op| (op.start, op.end > op.start, op.order));

    let mut output = Vec::new();
    let mut cursor = 0usize;
    for op in ops {
        if op.start < cursor {
            return Err(anyhow!(
                "patch hunks overlap or target an already replaced line"
            ));
        }
        output.extend_from_slice(&lines[cursor..op.start]);
        output.extend(op.replacement);
        cursor = op.end;
    }
    output.extend_from_slice(&lines[cursor..]);
    Ok(join_text_lines(&output, final_newline))
}

fn parse_binary_patch(patch_text: &str) -> Result<Vec<BinaryHunk>> {
    if patch_text
        .lines()
        .any(|line| line.trim() == "*** Begin Patch")
    {
        return Err(anyhow!(
            "old apply_patch envelope format is not supported; pass filePath separately and use binary patchText"
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

fn hunk_to_operation(hunk: &Hunk, lines: &[String]) -> Result<Operation> {
    let (start, end) = match hunk.anchor {
        Anchor::Replace { start, end } => {
            if start > end {
                return Err(anyhow!("hunk range start must be <= end: {start} {end}"));
            }
            ensure_line_exists(start, lines.len())?;
            ensure_line_exists(end, lines.len())?;
            (start - 1, end)
        }
        Anchor::Delete { start, end } => {
            if start > end {
                return Err(anyhow!("hunk range start must be <= end: {start} {end}"));
            }
            ensure_line_exists(start, lines.len())?;
            ensure_line_exists(end, lines.len())?;
            (start - 1, end)
        }
        Anchor::Insert {
            target: InsertTarget::Start,
        } => (0, 0),
        Anchor::Insert {
            target: InsertTarget::After(line),
        } => {
            ensure_insert_line(line, lines.len())?;
            (line, line)
        }
        Anchor::Insert {
            target: InsertTarget::End,
        } => {
            let line = lines.len() + 1;
            (line - 1, line - 1)
        }
    };

    let mut replacement = Vec::new();
    for body in &hunk.body {
        match body {
            BodyLine::Literal(text) => replacement.push(text.clone()),
            BodyLine::Copy { start, end } => {
                ensure_line_exists(*start, lines.len())?;
                ensure_line_exists(*end, lines.len())?;
                replacement.extend_from_slice(&lines[start - 1..*end]);
            }
        }
    }

    Ok(Operation {
        start,
        end,
        replacement,
        order: hunk.order,
    })
}

fn ensure_line_exists(line: usize, total: usize) -> Result<()> {
    if line > total {
        return Err(anyhow!(
            "line {line} is out of range for this file ({total} lines)"
        ));
    }
    Ok(())
}

fn ensure_insert_line(line: usize, total: usize) -> Result<()> {
    if line > total {
        return Err(anyhow!(
            "insert line {line} is out of range for this file ({total} lines); use insert 0 for the start or insert -1 for the end"
        ));
    }
    Ok(())
}

fn split_text_lines(text: &str) -> (Vec<String>, bool) {
    let final_newline = text.ends_with('\n');
    let body = if final_newline {
        &text[..text.len().saturating_sub(1)]
    } else {
        text
    };
    if body.is_empty() {
        return (Vec::new(), final_newline);
    }
    (
        body.split('\n').map(str::to_string).collect(),
        final_newline,
    )
}

fn join_text_lines(lines: &[String], final_newline: bool) -> String {
    let mut text = lines.join("\n");
    if final_newline {
        text.push('\n');
    }
    text
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
        diff,
        before: before.to_string(),
        after: after.to_string(),
        additions,
        deletions,
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

fn bytes_hex(bytes: &[u8]) -> String {
    const MAX: usize = 128;
    let mut value = bytes
        .iter()
        .take(MAX)
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ");
    if bytes.len() > MAX {
        value.push_str(" ...");
    }
    value
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_patch_replaces_and_reuses_original_lines() {
        let text = "a\nb\nc\n";
        let hunks = parse_line_patch("replace 1 3\ncopy 1 1\n+B\ncopy 3 3").unwrap();
        assert_eq!(apply_hunks(text, &hunks).unwrap(), "a\nB\nc\n");
    }

    #[test]
    fn line_patch_deletes_empty_body_range() {
        let text = "a\nb\nc\n";
        let hunks = parse_line_patch("delete 2 2").unwrap();
        assert_eq!(apply_hunks(text, &hunks).unwrap(), "a\nc\n");
    }

    #[test]
    fn line_patch_preserves_missing_final_newline() {
        let text = "a\nb";
        let hunks = parse_line_patch("replace 2 2\n+B").unwrap();
        assert_eq!(apply_hunks(text, &hunks).unwrap(), "a\nB");
    }

    #[test]
    fn insert_minus_one_appends() {
        let text = "a\nb\n";
        let hunks = parse_line_patch("insert -1\n+c").unwrap();
        assert_eq!(apply_hunks(text, &hunks).unwrap(), "a\nb\nc\n");
    }

    #[test]
    fn insert_minus_one_appends_multiple_lines() {
        let text = "a\nb\n";
        let hunks = parse_line_patch("insert -1\n+c\n+d").unwrap();
        assert_eq!(apply_hunks(text, &hunks).unwrap(), "a\nb\nc\nd\n");
    }

    #[test]
    fn insert_zero_before_replaced_line_is_allowed() {
        let text = "a\nb\n";
        let hunks = parse_line_patch("replace 1 1\n+A\ninsert 0\n+top").unwrap();
        assert_eq!(apply_hunks(text, &hunks).unwrap(), "top\nA\nb\n");
    }
}
