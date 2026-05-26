use crate::{ToolContext, ToolResult};
use anyhow::{anyhow, Context, Result};
use diffy::patch_set::{FileOperation, FilePatch, ParseOptions, PatchKind, PatchSet};
use diffy::{apply as diffy_apply, create_patch as diffy_create_patch};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
pub struct ApplyOptions {
    #[serde(rename = "patchText")]
    pub patch_text: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiffOptions {
    #[serde(rename = "patchText")]
    pub patch_text: String,
    #[serde(default)]
    pub strip: Option<usize>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "movePath")]
    pub move_path: Option<String>,
}

pub async fn apply_patch(options: ApplyOptions, ctx: &ToolContext) -> Result<ToolResult> {
    if options.patch_text.trim().is_empty() {
        return Err(anyhow!("patchText is required"));
    }

    let files = apply_tool_patch(ctx, &options.patch_text)?;
    Ok(result_from_files(files))
}

pub async fn apply_diffy(options: DiffOptions, ctx: &ToolContext) -> Result<ToolResult> {
    if options.patch_text.trim().is_empty() {
        return Err(anyhow!("patchText is required"));
    }

    let files = apply_unified_diff(ctx, &options.patch_text, options.strip)?;
    Ok(result_from_files(files))
}

fn result_from_files(files: Vec<PatchFile>) -> ToolResult {
    let summary = files
        .iter()
        .map(|file| match file.kind.as_str() {
            "add" => format!("A {}", file.relative_path),
            "delete" => format!("D {}", file.relative_path),
            "move" => format!("R {}", file.relative_path),
            "copy" => format!("C {}", file.relative_path),
            _ => format!("M {}", file.relative_path),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let output = format!("Success. Updated the following files:\n{summary}");
    let diff = files
        .iter()
        .map(|file| file.diff.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    ToolResult {
        title: output.clone(),
        metadata: json!({ "diff": diff, "files": files, "diagnostics": {} }),
        output,
    }
}

fn apply_unified_diff(
    ctx: &ToolContext,
    patch_text: &str,
    strip: Option<usize>,
) -> Result<Vec<PatchFile>> {
    let mut files = Vec::new();
    let patches = parse_file_patches(patch_text)?;

    for file_patch in patches {
        let op = file_patch.operation();
        let operation = op.strip_prefix(strip.unwrap_or_else(|| inferred_strip(op)));

        let file = match operation {
            FileOperation::Create(path) => {
                let target = resolve_patch_path(ctx, path.as_ref());
                let before = String::new();
                let after = apply_text_patch(&before, file_patch.patch())?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, &after)?;
                patch_file(ctx, &target, "add", &before, &after, None)
            }
            FileOperation::Delete(path) => {
                let target = resolve_patch_path(ctx, path.as_ref());
                let before = fs::read_to_string(&target)?;
                let _ = apply_text_patch(&before, file_patch.patch())?;
                fs::remove_file(&target)?;
                patch_file(ctx, &target, "delete", &before, "", None)
            }
            FileOperation::Modify { original, modified } => {
                let source = resolve_patch_path(ctx, original.as_ref());
                let target = resolve_patch_path(ctx, modified.as_ref());
                let before = fs::read_to_string(&source)?;
                let after = apply_text_patch(&before, file_patch.patch())?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, &after)?;
                if source != target {
                    fs::remove_file(&source)?;
                }
                let move_path = (source != target).then(|| target.to_string_lossy().into_owned());
                let kind = if move_path.is_some() {
                    "move"
                } else {
                    "update"
                };
                patch_file(ctx, &target, kind, &before, &after, move_path)
            }
            FileOperation::Rename { from, to } => {
                let source = resolve_patch_path(ctx, from.as_ref());
                let target = resolve_patch_path(ctx, to.as_ref());
                let before = fs::read_to_string(&source)?;
                let after = apply_text_patch(&before, file_patch.patch())?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, &after)?;
                if source != target {
                    fs::remove_file(&source)?;
                }
                patch_file(
                    ctx,
                    &target,
                    "move",
                    &before,
                    &after,
                    Some(target.to_string_lossy().into_owned()),
                )
            }
            FileOperation::Copy { from, to } => {
                let source = resolve_patch_path(ctx, from.as_ref());
                let target = resolve_patch_path(ctx, to.as_ref());
                let before = fs::read_to_string(&source)?;
                let after = apply_text_patch(&before, file_patch.patch())?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, &after)?;
                patch_file(
                    ctx,
                    &target,
                    "copy",
                    &before,
                    &after,
                    Some(source.to_string_lossy().into_owned()),
                )
            }
        };

        files.push(file);
    }

    if files.is_empty() {
        return Err(anyhow!("diffy patch did not contain any file changes"));
    }

    Ok(files)
}

fn apply_binary_write(
    ctx: &ToolContext,
    path: &str,
    lines: &[&str],
    mut i: usize,
    end: usize,
) -> Result<(PatchFile, usize)> {
    let target = resolve_patch_path(ctx, path);
    let before = fs::read(&target).unwrap_or_default();
    let mut text = String::new();
    while i < end {
        let line = lines[i];
        if line.starts_with("*** ") {
            break;
        }
        let body = line.strip_prefix('+').unwrap_or(line);
        text.push_str(body);
        text.push('\n');
        i += 1;
    }
    let after = decode_hex(&text)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, &after)?;
    Ok((
        binary_patch_file(ctx, &target, "binary-write", &before, &after, None),
        i,
    ))
}

fn apply_binary_update(
    ctx: &ToolContext,
    path: &str,
    lines: &[&str],
    mut i: usize,
    end: usize,
) -> Result<(PatchFile, usize)> {
    let target = resolve_patch_path(ctx, path);
    let before = fs::read(&target)?;
    let mut offset: Option<usize> = None;
    let mut old = None;
    let mut new = None;
    while i < end {
        let line = lines[i];
        if line.starts_with("*** Binary ")
            || line.starts_with("*** Add File:")
            || line.starts_with("*** Delete File:")
            || line.starts_with("*** Update File:")
        {
            break;
        }
        if let Some(value) = line.strip_prefix("*** Offset:") {
            offset = Some(value.trim().parse()?);
        } else if let Some(value) = line.strip_prefix("*** Old Bytes:") {
            old = Some(decode_hex(value.trim())?);
        } else if let Some(value) = line.strip_prefix("*** New Bytes:") {
            new = Some(decode_hex(value.trim())?);
        } else if !line.trim().is_empty() {
            return Err(anyhow!("unsupported binary update line: {line}"));
        }
        i += 1;
    }
    let offset = offset.ok_or_else(|| anyhow!("binary update requires *** Offset:"))?;
    let old = old.ok_or_else(|| anyhow!("binary update requires *** Old Bytes:"))?;
    let new = new.ok_or_else(|| anyhow!("binary update requires *** New Bytes:"))?;
    let end_offset = offset + old.len();
    if end_offset > before.len() {
        return Err(anyhow!(
            "binary update range {}..{} is out of bounds for {} bytes",
            offset,
            end_offset,
            before.len()
        ));
    }
    if before[offset..end_offset] != old {
        return Err(anyhow!(
            "binary update old bytes did not match at offset {offset}"
        ));
    }
    let mut after = before.clone();
    after.splice(offset..end_offset, new.iter().copied());
    fs::write(&target, &after)?;
    Ok((
        binary_patch_file(ctx, &target, "binary-update", &before, &after, None),
        i,
    ))
}

fn parse_file_patches(patch_text: &str) -> Result<Vec<FilePatch<'_, str>>> {
    let git = PatchSet::parse(patch_text, ParseOptions::gitdiff())
        .collect::<std::result::Result<Vec<_>, _>>();
    match git {
        Ok(patches) if !patches.is_empty() => Ok(patches),
        _ => PatchSet::parse(patch_text, ParseOptions::unidiff())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to parse unified diff"),
    }
}

fn apply_text_patch(base: &str, patch: &PatchKind<'_, str>) -> Result<String> {
    let patch = patch
        .as_text()
        .ok_or_else(|| anyhow!("binary patches are not supported"))?;
    diffy_apply(base, patch).map_err(|err| anyhow!("diffy apply failed: {err}"))
}

fn resolve_patch_path(ctx: &ToolContext, path: &str) -> PathBuf {
    ctx.resolve(path)
}

fn inferred_strip(operation: &FileOperation<'_, str>) -> usize {
    match operation {
        FileOperation::Rename { .. } | FileOperation::Copy { .. } => 0,
        FileOperation::Create(path) => has_git_prefix(path.as_ref()).then_some(1).unwrap_or(0),
        FileOperation::Delete(path) => has_git_prefix(path.as_ref()).then_some(1).unwrap_or(0),
        FileOperation::Modify { original, modified } => {
            if has_git_prefix(original.as_ref()) || has_git_prefix(modified.as_ref()) {
                1
            } else {
                0
            }
        }
    }
}

fn has_git_prefix(path: &str) -> bool {
    matches!(path.split_once('/'), Some((prefix, _)) if prefix == "a" || prefix == "b")
}

fn apply_tool_patch(ctx: &ToolContext, patch: &str) -> Result<Vec<PatchFile>> {
    let lines = patch.trim().lines().collect::<Vec<_>>();
    let begin = lines
        .iter()
        .position(|line| line.trim() == "*** Begin Patch")
        .ok_or_else(|| {
            anyhow!(
                "apply_patch verification failed: Invalid patch format: missing Begin/End markers"
            )
        })?;
    let end = lines
        .iter()
        .position(|line| line.trim() == "*** End Patch")
        .ok_or_else(|| {
            anyhow!("apply_patch verification failed: Invalid patch format: missing End marker")
        })?;

    let mut files = Vec::new();
    let mut i = begin + 1;
    while i < end {
        let line = lines[i];
        if let Some(path) = line.strip_prefix("*** Binary Write File:") {
            let (file, next) = apply_binary_write(ctx, path.trim(), &lines, i + 1, end)?;
            files.push(file);
            i = next;
        } else if let Some(path) = line.strip_prefix("*** Binary Update File:") {
            let (file, next) = apply_binary_update(ctx, path.trim(), &lines, i + 1, end)?;
            files.push(file);
            i = next;
        } else if let Some(path) = line.strip_prefix("*** Add File:") {
            let (file, next) = apply_add(ctx, path.trim(), &lines, i + 1, end)?;
            files.push(file);
            i = next;
        } else if let Some(path) = line.strip_prefix("*** Delete File:") {
            files.push(apply_delete(ctx, path.trim())?);
            i += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File:") {
            let (file, next) = apply_update(ctx, path.trim(), &lines, i + 1, end)?;
            files.push(file);
            i = next;
        } else {
            i += 1;
        }
    }

    Ok(files)
}

fn apply_add(
    ctx: &ToolContext,
    path: &str,
    lines: &[&str],
    mut i: usize,
    end: usize,
) -> Result<(PatchFile, usize)> {
    let target = ctx.resolve(path);
    let mut after = String::new();
    while i < end && !lines[i].starts_with("***") {
        if let Some(text) = lines[i].strip_prefix('+') {
            after.push_str(text);
            after.push('\n');
        }
        i += 1;
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, &after)?;
    Ok((patch_file(ctx, &target, "add", "", &after, None), i))
}

fn apply_delete(ctx: &ToolContext, path: &str) -> Result<PatchFile> {
    let target = ctx.resolve(path);
    let before = fs::read_to_string(&target)?;
    fs::remove_file(&target)?;
    Ok(patch_file(ctx, &target, "delete", &before, "", None))
}

fn apply_update(
    ctx: &ToolContext,
    path: &str,
    lines: &[&str],
    mut i: usize,
    end: usize,
) -> Result<(PatchFile, usize)> {
    let source = ctx.resolve(path);
    let before = fs::read_to_string(&source)?;
    let mut after = before.clone();
    let mut target = source.clone();
    let mut old = Vec::new();
    let mut new = Vec::new();

    if i < end && lines[i].starts_with("*** Move to:") {
        target = ctx.resolve(lines[i].trim_start_matches("*** Move to:").trim());
        i += 1;
    }

    while i < end && !lines[i].starts_with("***") {
        let line = lines[i];
        if line.starts_with("@@") {
            after = apply_chunk(&after, &old, &new)?;
            old.clear();
            new.clear();
        } else if let Some(text) = line.strip_prefix(' ') {
            old.push(text.to_string());
            new.push(text.to_string());
        } else if let Some(text) = line.strip_prefix('-') {
            old.push(text.to_string());
        } else if let Some(text) = line.strip_prefix('+') {
            new.push(text.to_string());
        }
        i += 1;
    }

    after = apply_chunk(&after, &old, &new)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, &after)?;
    if target != source {
        fs::remove_file(&source)?;
    }

    let move_path = (target != source).then(|| target.to_string_lossy().into_owned());
    let kind = if move_path.is_some() {
        "move"
    } else {
        "update"
    };
    Ok((
        patch_file(ctx, &target, kind, &before, &after, move_path),
        i,
    ))
}

fn apply_chunk(content: &str, old: &[String], new: &[String]) -> Result<String> {
    if old.is_empty() && new.is_empty() {
        return Ok(content.to_string());
    }
    let old_text = chunk_text(old);
    let new_text = chunk_text(new);
    let mut next = content.to_string();
    if let Some(pos) = content.find(&old_text) {
        next.replace_range(pos..pos + old_text.len(), &new_text);
        let patch = diffy_create_patch(content, &next);
        return diffy_apply(content, &patch).map_err(|err| anyhow!("diffy apply failed: {err}"));
    }

    let trimmed = old_text.trim_end_matches('\n');
    if !trimmed.is_empty() {
        if let Some(pos) = content.find(trimmed) {
            next.replace_range(pos..pos + trimmed.len(), new_text.trim_end_matches('\n'));
            let patch = diffy_create_patch(content, &next);
            return diffy_apply(content, &patch)
                .map_err(|err| anyhow!("diffy apply failed: {err}"));
        }
    }

    Err(anyhow!(
        "apply_patch verification failed: patch hunk did not match file content"
    ))
}

fn chunk_text(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

fn patch_file(
    ctx: &ToolContext,
    path: &Path,
    kind: &str,
    before: &str,
    after: &str,
    move_path: Option<String>,
) -> PatchFile {
    let diff = diff_text(path, before, after);
    let additions = count_diff_lines(&diff, '+');
    let deletions = count_diff_lines(&diff, '-');
    PatchFile {
        file_path: path.to_string_lossy().into_owned(),
        relative_path: ctx.title(path),
        kind: kind.to_string(),
        diff,
        before: before.to_string(),
        after: after.to_string(),
        additions,
        deletions,
        move_path,
    }
}

fn binary_patch_file(
    ctx: &ToolContext,
    path: &Path,
    kind: &str,
    before: &[u8],
    after: &[u8],
    move_path: Option<String>,
) -> PatchFile {
    let before_hex = bytes_hex(before);
    let after_hex = bytes_hex(after);
    let diff = format!(
        "Binary {kind}: {}\n- {} bytes: {}\n+ {} bytes: {}\n",
        path.display(),
        before.len(),
        before_hex,
        after.len(),
        after_hex
    );
    PatchFile {
        file_path: path.to_string_lossy().into_owned(),
        relative_path: ctx.title(path),
        kind: kind.to_string(),
        diff,
        before: before_hex,
        after: after_hex,
        additions: after.len(),
        deletions: before.len(),
        move_path,
    }
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
