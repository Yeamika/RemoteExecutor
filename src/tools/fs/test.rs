use crate::{
    glob_paths, read_path, stat_path, GlobOptions, ReadMode, ReadOptions, StatOptions, ToolContext,
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

    let glob_output = output.output["text"].as_str().unwrap();
    assert_eq!(glob_output.lines().count(), 1);
    assert!(glob_output.contains("a.rs"));
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
    assert!(output.output["info"]
        .as_str()
        .unwrap()
        .contains("Use offset=129"));
    assert!(output.output["text"].as_str().unwrap().contains("00000001"));
    assert_eq!(output.output["message"].as_str().unwrap(), "");
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

    assert!(output.output["text"].as_str().unwrap().contains("2: two"));
    assert!(output.output["info"]
        .as_str()
        .unwrap()
        .contains("Use offset=3"));
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
    assert_eq!(output.metadata["hashCode"], hash_code);
}
