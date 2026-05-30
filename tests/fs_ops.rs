use remote_executor::{
    apply_patch, glob_paths, read_path, stat_path, ApplyOptions, GlobOptions, PatchMode, ReadMode,
    ReadOptions, StatOptions, ToolContext,
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
            hash_check_mode: false,
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
            hash_check_mode: false,
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
            hash_check_mode: false,
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
            hash_check_mode: false,
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

#[test]
fn read_path_returns_hash_code_when_requested() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.txt");
    fs::write(&path, "one\ntwo\n").unwrap();

    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    let output = read_path(
        ReadOptions {
            file_path: path.clone(),
            mode: None,
            offset: Some(1),
            limit: Some(1),
            hash_check_mode: true,
        },
        &ctx,
    )
    .unwrap();

    let hash_code = output.metadata["hashCode"].as_str().unwrap();
    assert!(hash_code.starts_with("sha256:"));
    assert_eq!(hash_code.len(), "sha256:".len() + 64);
    assert!(output.output.contains(hash_code));
}

#[tokio::test]
async fn apply_patch_applies_line_number_patch_with_hash_check() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    let read = read_path(
        ReadOptions {
            file_path: path.clone(),
            mode: None,
            offset: None,
            limit: None,
            hash_check_mode: true,
        },
        &ctx,
    )
    .unwrap();
    let hash_code = read.metadata["hashCode"].as_str().unwrap().to_string();

    let result = apply_patch(
        ApplyOptions {
            file_path: path.clone(),
            patch_text: "replace 2 2\n+TWO\ninsert -1\n+four".to_string(),
            patch_mode: PatchMode::Text,
            hash_check_mode: true,
            hash_code: Some(hash_code),
        },
        &ctx,
    )
    .await
    .unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "one\nTWO\nthree\nfour\n"
    );
    let new_hash = result.metadata["hashCode"].as_str().unwrap();
    assert!(new_hash.starts_with("sha256:"));
    assert!(result.output.contains(new_hash));
}

#[tokio::test]
async fn apply_patch_applies_binary_offset_patch_with_hash_check() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.bin");
    fs::write(&path, [0x00, 0x01, 0x02, 0x03, 0x04]).unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    let read = read_path(
        ReadOptions {
            file_path: path.clone(),
            mode: Some(ReadMode::Binary),
            offset: None,
            limit: None,
            hash_check_mode: true,
        },
        &ctx,
    )
    .unwrap();
    let hash_code = read.metadata["hashCode"].as_str().unwrap().to_string();

    let result = apply_patch(
        ApplyOptions {
            file_path: path.clone(),
            patch_text: "insert 0\n+FE\nreplace 1 2\n+AA BB\ndelete 4 1\ninsert -1\n+CC\n+DD"
                .to_string(),
            patch_mode: PatchMode::Binary,
            hash_check_mode: true,
            hash_code: Some(hash_code),
        },
        &ctx,
    )
    .await
    .unwrap();

    assert_eq!(
        fs::read(&path).unwrap(),
        [0xFE, 0x00, 0xAA, 0xBB, 0x03, 0xCC, 0xDD]
    );
    let new_hash = result.metadata["hashCode"].as_str().unwrap();
    assert!(new_hash.starts_with("sha256:"));
    assert!(result.metadata["file"]["type"] == "binary-update");
}

#[tokio::test]
async fn apply_patch_binary_rejects_copy_body_lines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.bin");
    fs::write(&path, [0x00, 0x01]).unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let err = apply_patch(
        ApplyOptions {
            file_path: path.clone(),
            patch_text: "replace 0 1\ncopy 0 1".to_string(),
            patch_mode: PatchMode::Binary,
            hash_check_mode: false,
            hash_code: None,
        },
        &ctx,
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(err.contains("copy body lines are not supported"), "{err}");
    assert_eq!(fs::read(path).unwrap(), [0x00, 0x01]);
}

#[tokio::test]
async fn apply_patch_rejects_stale_hash_without_writing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "one\ntwo\n").unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let err = apply_patch(
        ApplyOptions {
            file_path: path.clone(),
            patch_text: "replace 2 2\n+TWO".to_string(),
            patch_mode: PatchMode::Text,
            hash_check_mode: true,
            hash_code: Some(format!("sha256:{}", "0".repeat(64))),
        },
        &ctx,
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(err.contains("hash mismatch"), "{err}");
    assert_eq!(fs::read_to_string(path).unwrap(), "one\ntwo\n");
}

#[tokio::test]
async fn apply_patch_rejects_old_envelope_format() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "before\n").unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let err = apply_patch(
        ApplyOptions {
            file_path: path.clone(),
            patch_text:
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+after\n*** End Patch"
                    .to_string(),
            patch_mode: PatchMode::Text,
            hash_check_mode: false,
            hash_code: None,
        },
        &ctx,
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(err.contains("old apply_patch envelope"), "{err}");
    assert_eq!(fs::read_to_string(path).unwrap(), "before\n");
}
