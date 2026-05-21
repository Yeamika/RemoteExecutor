use remote_executor::{
    apply_diffy, apply_patch, glob_paths, read_path, ApplyOptions, DiffOptions, GlobOptions,
    ReadOptions, ToolContext,
};
use std::fs;
use tempfile::tempdir;

#[test]
fn glob_paths_uses_pattern_matching() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.path().join("b.txt"), "hello\n").unwrap();

    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    let output = glob_paths(
        GlobOptions {
            pattern: "*.rs".to_string(),
            path: Some(dir.path().to_path_buf()),
        },
        &ctx,
    )
    .unwrap();

    assert_eq!(output.output.lines().count(), 1);
    assert!(output.output.contains("a.rs"));
}

#[test]
fn read_path_reads_file_with_lines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    let output = read_path(
        ReadOptions {
            file_path: path.clone(),
            offset: Some(2),
            limit: Some(1),
        },
        &ctx,
    )
    .unwrap();

    assert!(output.output.contains("2: two"));
}

#[tokio::test]
async fn apply_patch_can_apply_patch() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "before\n").unwrap();

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch\n",
        path.display()
    );

    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    apply_patch(ApplyOptions { patch_text: patch }, &ctx)
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "after\n");
}

#[tokio::test]
async fn diffy_can_apply_unified_diff() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "before\n").unwrap();

    let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n";
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    apply_diffy(
        DiffOptions {
            patch_text: patch.to_string(),
            strip: None,
        },
        &ctx,
    )
    .await
    .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "after\n");
}
