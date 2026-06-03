# FileAction Tool

Tool name: `FileAction`

All examples in this file use the raw JSONL RPC shape.

Request shape:

```json
{
  "id": 1,
  "tool": "FileAction",
  "params": {
    "mode": "patch",
    "filePath": "src/foo.rs",
    "newFilePath": "src/bar.rs",
    "patchText": "@@ -1 +1 @@\n-old\n+new\n",
    "content": "new file content",
    "patchMode": "text",
    "hashCheckMode": false,
    "hashCode": "sha256:..."
  },
  "directory": "/repo"
}
```

Modes:

| `mode` | Required params | Notes |
|---|---|---|
| `patch` | `filePath`, `patchText` | Updates one existing file. |
| `create` | `filePath`, `content` | Fails if the target already exists. |
| `delete` | `filePath` | Deletes one existing file. |
| `rename` | `filePath`, `newFilePath` | Fails if the destination already exists. |

`patchMode` defaults to `text`. Use `binary` for byte-offset patches and binary create content.

Text patches use unified diff and are applied through `diffy::Patch::from_str` and `diffy::apply`. If `patchText` starts with `@@`, FileAction adds internal file headers before passing it to `diffy`, so callers do not need to include `---` and `+++` lines.

Captured text patch request:

```json
{
  "id": 11,
  "tool": "FileAction",
  "params": {
    "mode": "patch",
    "filePath": "created.txt",
    "patchMode": "text",
    "patchText": "@@ -1 +1 @@\n-hello\n+HELLO\n",
    "hashCheckMode": true,
    "hashCode": "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
  },
  "directory": "/tmp/re-doc-action"
}
```

## Binary Patch Mode

Captured binary patch request:

```json
{
  "id": 14,
  "tool": "FileAction",
  "params": {
    "mode": "patch",
    "filePath": "data.bin",
    "patchMode": "binary",
    "patchText": "replace 10 2\n+AA BB"
  },
  "directory": "/tmp/re-doc-action"
}
```

Binary hunk headers use 0-based byte offsets:

| Header | Meaning |
|---|---|
| `replace OFFSET LEN` | Replace `LEN` bytes starting at `OFFSET`. |
| `delete OFFSET LEN` | Delete `LEN` bytes starting at `OFFSET`. |
| `insert 0` | Insert at the start. |
| `insert -1` | Insert at the end. |
| `insert OFFSET` | Insert at byte offset `OFFSET`. |

Binary body lines must be `+HEX`. Multiple body lines concatenate. `copy` body lines are rejected in binary mode.

## Hash Checking

When `hashCheckMode` is true for an existing file action, `hashCode` must match the current file hash. Mutating actions that leave a file on disk return the new `hashCode`.

## RPC Examples

The human-readable text is in `result.output`. Structured fields are in `result.metadata`.

Patch request:

```json
{
  "id": 11,
  "tool": "FileAction",
  "params": {
    "mode": "patch",
    "filePath": "created.txt",
    "patchMode": "text",
    "patchText": "@@ -1 +1 @@\n-hello\n+HELLO\n",
    "hashCheckMode": true,
    "hashCode": "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
  },
  "directory": "/tmp/re-doc-action"
}
```

Patch response:

```json
{
  "id": 11,
  "ok": true,
  "result": {
    "metadata": {
      "diagnostics": {},
      "file": {
        "additions": 1,
        "deletions": 1,
        "filePath": "/tmp/re-doc-action/created.txt",
        "relativePath": "created.txt",
        "type": "update"
      },
      "hashCode": "sha256:3b09aeb6f5f5336beb205d7f720371bc927cd46c21922e334d47ba264acb5ba4"
    },
    "output": { "message": "", "text": "Success. Updated file:\nM created.txt\nhashCode: sha256:3b09aeb6f5f5336beb205d7f720371bc927cd46c21922e334d47ba264acb5ba4", "info": "" }
  },
  "executor": "local"
}
```

Create request:

```json
{
  "id": 10,
  "tool": "FileAction",
  "params": {
    "mode": "create",
    "filePath": "created.txt",
    "content": "hello\n",
    "hashCheckMode": true
  },
  "directory": "/tmp/re-doc-action"
}
```

Create response:

```json
{
  "id": 10,
  "ok": true,
  "result": {
    "metadata": {
      "diagnostics": {},
      "file": {
        "additions": 6,
        "deletions": 0,
        "filePath": "/tmp/re-doc-action/created.txt",
        "relativePath": "created.txt",
        "type": "create"
      },
      "hashCode": "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
    },
    "output": { "message": "", "text": "Success. Created file:\nC created.txt\nhashCode: sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03", "info": "" }
  },
  "executor": "local"
}
```

Delete request:

```json
{
  "id": 13,
  "tool": "FileAction",
  "params": {
    "mode": "delete",
    "filePath": "renamed.txt"
  },
  "directory": "/tmp/re-doc-action"
}
```

Delete response:

```json
{
  "id": 13,
  "ok": true,
  "result": {
    "metadata": {
      "diagnostics": {},
      "file": {
        "additions": 0,
        "deletions": 6,
        "filePath": "/tmp/re-doc-action/renamed.txt",
        "relativePath": "renamed.txt",
        "type": "delete"
      }
    },
    "output": { "message": "", "text": "Success. Deleted file:\nD renamed.txt", "info": "" }
  },
  "executor": "local"
}
```

Rename request:

```json
{
  "id": 12,
  "tool": "FileAction",
  "params": {
    "mode": "rename",
    "filePath": "created.txt",
    "newFilePath": "renamed.txt",
    "hashCheckMode": true,
    "hashCode": "sha256:3b09aeb6f5f5336beb205d7f720371bc927cd46c21922e334d47ba264acb5ba4"
  },
  "directory": "/tmp/re-doc-action"
}
```

Rename response:

```json
{
  "id": 12,
  "ok": true,
  "result": {
    "metadata": {
      "diagnostics": {},
      "file": {
        "additions": 0,
        "deletions": 0,
        "filePath": "/tmp/re-doc-action/created.txt",
        "newFilePath": "/tmp/re-doc-action/renamed.txt",
        "newRelativePath": "renamed.txt",
        "relativePath": "created.txt",
        "type": "rename"
      },
      "hashCode": "sha256:3b09aeb6f5f5336beb205d7f720371bc927cd46c21922e334d47ba264acb5ba4"
    },
    "output": { "message": "", "text": "Success. Renamed file:\nR created.txt -> renamed.txt\nhashCode: sha256:3b09aeb6f5f5336beb205d7f720371bc927cd46c21922e334d47ba264acb5ba4", "info": "" }
  },
  "executor": "local"
}
```

`newFilePath` and `newRelativePath` are only present for rename. `hashCode` is only present when requested and applicable.

FileAction must not return full before/after file contents in either `result.output` or `result.metadata`.