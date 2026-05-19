use crate::{ToolContext, ToolResult};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Deserialize)]
pub struct ApplyOptions {
    #[serde(rename = "patchText", alias = "patch")]
    pub patch_text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyOutput {
    pub files: Vec<PatchFile>,
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
    let summary = files
        .iter()
        .map(|file| match file.kind.as_str() {
            "add" => format!("A {}", file.relative_path),
            "delete" => format!("D {}", file.relative_path),
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

    Ok(ToolResult {
        title: output.clone(),
        metadata: json!({ "diff": diff, "files": files, "diagnostics": {} }),
        output,
    })
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
        if let Some(path) = line.strip_prefix("*** Add File:") {
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
    let additions = after.lines().count();
    Ok((
        patch_file(ctx, &target, "add", "", &after, additions, 0, None),
        i,
    ))
}

fn apply_delete(ctx: &ToolContext, path: &str) -> Result<PatchFile> {
    let target = ctx.resolve(path);
    let before = fs::read_to_string(&target)?;
    fs::remove_file(&target)?;
    Ok(patch_file(
        ctx,
        &target,
        "delete",
        &before,
        "",
        0,
        before.lines().count(),
        None,
    ))
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
            apply_chunk(&mut after, &old, &new)?;
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

    apply_chunk(&mut after, &old, &new)?;
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
        patch_file(
            ctx,
            &target,
            kind,
            &before,
            &after,
            count_added(&before, &after),
            count_deleted(&before, &after),
            move_path,
        ),
        i,
    ))
}

fn apply_chunk(content: &mut String, old: &[String], new: &[String]) -> Result<()> {
    if old.is_empty() && new.is_empty() {
        return Ok(());
    }
    let old_text = old.join("\n") + "\n";
    let new_text = new.join("\n") + "\n";
    if let Some(pos) = content.find(&old_text) {
        content.replace_range(pos..pos + old_text.len(), &new_text);
        return Ok(());
    }
    let trimmed = old_text.trim_end_matches('\n');
    if let Some(pos) = content.find(trimmed) {
        content.replace_range(pos..pos + trimmed.len(), new_text.trim_end_matches('\n'));
        return Ok(());
    }
    Err(anyhow!(
        "apply_patch verification failed: patch hunk did not match file content"
    ))
}

fn patch_file(
    ctx: &ToolContext,
    path: &Path,
    kind: &str,
    before: &str,
    after: &str,
    additions: usize,
    deletions: usize,
    move_path: Option<String>,
) -> PatchFile {
    PatchFile {
        file_path: path.to_string_lossy().into_owned(),
        relative_path: ctx.title(path),
        kind: kind.to_string(),
        diff: simple_diff(path, before, after),
        before: before.to_string(),
        after: after.to_string(),
        additions,
        deletions,
        move_path,
    }
}

fn simple_diff(path: &Path, before: &str, after: &str) -> String {
    format!(
        "--- {}\n+++ {}\n-{}\n+{}",
        path.display(),
        path.display(),
        before.trim_end(),
        after.trim_end()
    )
}

fn count_added(before: &str, after: &str) -> usize {
    after.lines().count().saturating_sub(before.lines().count())
}

fn count_deleted(before: &str, after: &str) -> usize {
    before.lines().count().saturating_sub(after.lines().count())
}
