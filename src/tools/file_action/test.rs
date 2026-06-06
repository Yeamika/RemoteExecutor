use crate::{
    file_action, read_path, FileActionMode, FileActionOptions, PatchMode, ReadMode, ReadOptions,
    ToolContext,
};
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

fn patch_action(
    file_path: impl Into<PathBuf>,
    patch_text: impl Into<String>,
    patch_mode: PatchMode,
    hash_check_mode: bool,
    hash_code: Option<String>,
) -> FileActionOptions {
    FileActionOptions {
        mode: FileActionMode::Patch,
        file_path: file_path.into(),
        new_file_path: None,
        patch_text: Some(patch_text.into()),
        content: None,
        patch_mode,
        hash_check_mode,
        hash_code,
    }
}

async fn apply_text_patch(initial: &str, patch_text: &str) -> String {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, initial).unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    file_action(
        patch_action(path.clone(), patch_text, PatchMode::Text, false, None),
        &ctx,
    )
    .await
    .unwrap();

    fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn file_action_rejects_non_patch_text_without_writing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "base\n").unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let result = file_action(
        patch_action(
            path.clone(),
            "this is not a unified diff",
            PatchMode::Text,
            false,
            None,
        ),
        &ctx,
    )
    .await;

    assert!(result.is_err(), "plain text patch should fail");
    assert_eq!(fs::read_to_string(path).unwrap(), "base\n");
}

#[tokio::test]
async fn file_action_applies_unified_diff_patch_with_hash_check() {
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

    let result = file_action(
        patch_action(
            path.clone(),
            "@@ -1,3 +1,4 @@\n one\n-two\n+TWO\n three\n+four\n",
            PatchMode::Text,
            true,
            Some(hash_code),
        ),
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
    assert!(result.output["text"].as_str().unwrap().contains(new_hash));
    assert!(result.metadata["file"].get("before").is_none());
    assert!(result.metadata["file"].get("after").is_none());
}

#[tokio::test]
async fn file_action_patch_result_does_not_return_full_file_contents() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    let tail = "secret-tail-line-that-should-not-be-returned";
    let mut content = String::from("one\n");
    for idx in 0..40 {
        content.push_str(&format!("middle-{idx}\n"));
    }
    content.push_str(tail);
    content.push('\n');
    fs::write(&path, content).unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let result = file_action(
        patch_action(
            path,
            "@@ -1 +1 @@\n-one\n+ONE\n",
            PatchMode::Text,
            false,
            None,
        ),
        &ctx,
    )
    .await
    .unwrap();

    let metadata = serde_json::to_string(&result.metadata).unwrap();
    assert!(result.metadata["file"].get("before").is_none());
    assert!(result.metadata["file"].get("after").is_none());
    assert!(
        !metadata.contains(tail),
        "metadata returned full file content"
    );
}

#[tokio::test]
async fn file_action_applies_binary_offset_patch_with_hash_check() {
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

    let result = file_action(
        patch_action(
            path.clone(),
            "insert 0\n+FE\nreplace 1 2\n+AA BB\ndelete 4 1\ninsert -1\n+CC\n+DD",
            PatchMode::Binary,
            true,
            Some(hash_code),
        ),
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
    assert!(result.metadata["file"].get("before").is_none());
    assert!(result.metadata["file"].get("after").is_none());
}

#[tokio::test]
async fn file_action_binary_rejects_copy_body_lines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.bin");
    fs::write(&path, [0x00, 0x01]).unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let err = file_action(
        patch_action(
            path.clone(),
            "replace 0 1\ncopy 0 1",
            PatchMode::Binary,
            false,
            None,
        ),
        &ctx,
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(err.contains("copy body lines are not supported"), "{err}");
    assert_eq!(fs::read(path).unwrap(), [0x00, 0x01]);
}

#[tokio::test]
async fn file_action_rejects_stale_hash_without_writing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "one\ntwo\n").unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let err = file_action(
        patch_action(
            path.clone(),
            "@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n",
            PatchMode::Text,
            true,
            Some(format!("sha256:{}", "0".repeat(64))),
        ),
        &ctx,
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(err.contains("hash mismatch"), "{err}");
    assert_eq!(fs::read_to_string(path).unwrap(), "one\ntwo\n");
}

#[tokio::test]
async fn file_action_rejects_old_patch_envelope_format() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("file.txt");
    fs::write(&path, "before\n").unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let err = file_action(
        patch_action(
            path.clone(),
            "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+after\n*** End Patch",
            PatchMode::Text,
            false,
            None,
        ),
        &ctx,
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(err.contains("old patch envelope"), "{err}");
    assert_eq!(fs::read_to_string(path).unwrap(), "before\n");
}

#[tokio::test]
async fn file_action_creates_text_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("created.txt");
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));

    let result = file_action(
        FileActionOptions {
            mode: FileActionMode::Create,
            file_path: path.clone(),
            new_file_path: None,
            patch_text: None,
            content: Some("hello\n".to_string()),
            patch_mode: PatchMode::Text,
            hash_check_mode: true,
            hash_code: None,
        },
        &ctx,
    )
    .await
    .unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
    assert_eq!(result.metadata["file"]["type"], "create");
    assert!(result.metadata["hashCode"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(result.metadata["file"].get("before").is_none());
    assert!(result.metadata["file"].get("after").is_none());
}

#[tokio::test]
async fn file_action_deletes_file_with_hash_check() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("delete.txt");
    fs::write(&path, "remove me\n").unwrap();
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

    let result = file_action(
        FileActionOptions {
            mode: FileActionMode::Delete,
            file_path: path.clone(),
            new_file_path: None,
            patch_text: None,
            content: None,
            patch_mode: PatchMode::Text,
            hash_check_mode: true,
            hash_code: Some(hash_code),
        },
        &ctx,
    )
    .await
    .unwrap();

    assert!(!path.exists());
    assert_eq!(result.metadata["file"]["type"], "delete");
    assert!(result.metadata.get("hashCode").is_none());
    assert!(result.metadata["file"].get("before").is_none());
    assert!(result.metadata["file"].get("after").is_none());
}

#[tokio::test]
async fn file_action_renames_file_with_hash_check() {
    let dir = tempdir().unwrap();
    let old_path = dir.path().join("old.txt");
    let new_path = dir.path().join("new.txt");
    fs::write(&old_path, "move me\n").unwrap();
    let ctx = ToolContext::new(Some(dir.path().to_path_buf()));
    let read = read_path(
        ReadOptions {
            file_path: old_path.clone(),
            mode: None,
            offset: None,
            limit: None,
            hash_check_mode: true,
        },
        &ctx,
    )
    .unwrap();
    let hash_code = read.metadata["hashCode"].as_str().unwrap().to_string();

    let result = file_action(
        FileActionOptions {
            mode: FileActionMode::Rename,
            file_path: old_path.clone(),
            new_file_path: Some(new_path.clone()),
            patch_text: None,
            content: None,
            patch_mode: PatchMode::Text,
            hash_check_mode: true,
            hash_code: Some(hash_code),
        },
        &ctx,
    )
    .await
    .unwrap();

    assert!(!old_path.exists());
    assert_eq!(fs::read_to_string(&new_path).unwrap(), "move me\n");
    assert_eq!(result.metadata["file"]["type"], "rename");
    assert!(result.metadata["file"]["newFilePath"]
        .as_str()
        .unwrap()
        .ends_with("new.txt"));
    assert!(result.metadata["hashCode"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}

#[tokio::test]
async fn file_action_applies_unified_diff_patch_with_diffy() {
    let output = apply_text_patch(
        "hello\nworld\n",
        "--- file.txt\n+++ file.txt\n@@ -1,2 +1,2 @@\n-hello\n+HELLO\n world\n",
    )
    .await;
    assert_eq!(output, "HELLO\nworld\n");
}

#[tokio::test]
async fn file_action_applies_hunk_only_unified_diff_patch() {
    let output = apply_text_patch(
        "hello\nworld\n",
        "@@ -1,2 +1,2 @@\n-hello\n+HELLO\n world\n",
    )
    .await;
    assert_eq!(output, "HELLO\nworld\n");
}
