use remote_executor::{
    apply_diffy, apply_patch, glob_paths, read_path, stat_path, ApplyOptions, DiffOptions,
    GlobOptions, ReadMode, ReadOptions, StatOptions, ToolContext,
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

#[tokio::test]
async fn apply_patch_can_write_and_update_binary() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.bin");

    let write = format!(
        "*** Begin Patch\n*** Binary Write File: {}\n+00 01 02 03\n*** End Patch\n",
        path.display()
    );
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    apply_patch(ApplyOptions { patch_text: write }, &ctx)
        .await
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), [0x00, 0x01, 0x02, 0x03]);

    let update = format!(
        "*** Begin Patch\n*** Binary Update File: {}\n*** Offset: 1\n*** Old Bytes: 01 02\n*** New Bytes: AA BB CC\n*** End Patch\n",
        path.display()
    );
    apply_patch(ApplyOptions { patch_text: update }, &ctx)
        .await
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), [0x00, 0xAA, 0xBB, 0xCC, 0x03]);
}

#[test]
fn read_path_reports_binary_offset_and_byte() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("binary.dat");
    fs::write(&path, [0x41, 0x42, 0x00, 0x43]).unwrap();

    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    let err = read_path(
        ReadOptions {
            file_path: path.clone(),
            mode: None,
            offset: None,
            limit: None,
        },
        &ctx,
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("offset 2"), "{err}");
    assert!(err.contains("0x00"), "{err}");
}

#[test]
fn read_path_binary_mode_returns_limited_hexdump() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("binary.dat");
    fs::write(&path, (0u8..=200).collect::<Vec<_>>()).unwrap();

    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    let output = read_path(
        ReadOptions {
            file_path: path.clone(),
            mode: Some(ReadMode::Binary),
            offset: Some(1),
            limit: Some(999),
        },
        &ctx,
    )
    .unwrap();

    assert!(output.output.contains("<type>binary</type>"));
    assert!(output.output.contains("00000001"));
    assert_eq!(output.metadata["length"], 128);
    assert_eq!(output.metadata["truncated"], true);
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
            mode: None,
            offset: Some(2),
            limit: Some(1),
        },
        &ctx,
    )
    .unwrap();

    assert!(output.output.contains("2: two"));
    let file = &output.metadata["file"];
    assert_eq!(file["kind"], "file");
    assert!(file["fileKey"].as_str().unwrap().contains(':'));
    assert!(file["canonicalPath"]
        .as_str()
        .unwrap()
        .ends_with("sample.txt"));
    assert_eq!(file["size"], 14);
    assert!(file["mtimeMs"].as_u64().is_some());
}

#[test]
fn stat_path_returns_file_stamp_for_files_and_missing_paths() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.txt");
    fs::write(&path, "one\n").unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let read = read_path(
        ReadOptions {
            file_path: path.clone(),
            mode: None,
            offset: None,
            limit: None,
        },
        &ctx,
    )
    .unwrap();
    let stat = stat_path(
        StatOptions {
            file_path: path.clone(),
        },
        &ctx,
    )
    .unwrap();
    assert_eq!(read.metadata["file"], stat.metadata["file"]);

    let missing = stat_path(
        StatOptions {
            file_path: dir.path().join("missing.txt"),
        },
        &ctx,
    )
    .unwrap();
    assert_eq!(missing.metadata["file"]["kind"], "missing");
    assert!(missing.metadata["file"]["fileKey"]
        .as_str()
        .unwrap()
        .starts_with("missing:"));
}

#[cfg(unix)]
#[test]
fn file_stamp_uses_physical_identity_for_hard_links() {
    let dir = tempdir().unwrap();
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    fs::write(&first, "same inode\n").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let first = stat_path(
        StatOptions {
            file_path: first.clone(),
        },
        &ctx,
    )
    .unwrap();
    let second = stat_path(
        StatOptions {
            file_path: second.clone(),
        },
        &ctx,
    )
    .unwrap();

    assert_eq!(
        first.metadata["file"]["fileKey"],
        second.metadata["file"]["fileKey"]
    );
    assert_ne!(
        first.metadata["file"]["canonicalPath"],
        second.metadata["file"]["canonicalPath"]
    );
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
