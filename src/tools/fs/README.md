# FS Tools

Tools: `glob`, `read`, `stat`

All examples use the raw JSONL RPC request/response shape.

## `glob`

Request:

```json
{"id":2,"tool":"glob","params":{"pattern":"src/*.rs"},"directory":"/tmp/re-doc-sample"}
```

Response:

```json
{
  "id": 2,
  "ok": true,
  "result": {
    "metadata": {
      "count": 1,
      "truncated": false
    },
    "output": { "message": "", "text": "/tmp/re-doc-sample/src/lib.rs", "info": "" }
  },
  "executor": "local"
}
```

## `read` Text File

Request:

```json
{"id":1,"tool":"read","params":{"filePath":"src/readme.txt","hashCheckMode":true},"directory":"/tmp/re-doc-sample"}
```

Response:

```json
{
  "id": 1,
  "ok": true,
  "result": {
    "metadata": {
      "file": {
        "canonicalPath": "/tmp/re-doc-sample/src/readme.txt",
        "fileKey": "file-id:Inode { device_id: 138, inode_number: 412780 }",
        "kind": "file",
        "mtimeMs": 1780466942043,
        "size": 12
      },
      "hashCode": "sha256:4a1e67f2fe1d1cc7b31d0ca2ec441da4778203a036a77da10344c85e24ff0f92"
    },
    "output": {
      "message": "",
      "text": "1: hello\n2: world",
      "info": "total 2 lines"
    }
  },
  "executor": "local"
}
```

## `read` Directory

Request:

```json
{"id":7,"tool":"read","params":{"filePath":"src"},"directory":"/tmp/re-doc-sample"}
```

Response:

```json
{
  "id": 7,
  "ok": true,
  "result": {
    "metadata": {
      "file": {
        "canonicalPath": "/tmp/re-doc-sample/src",
        "fileKey": "file-id:Inode { device_id: 138, inode_number: 412777 }",
        "kind": "directory",
        "mtimeMs": 1780466942043
      }
    },
    "output": {
      "message": "",
      "text": "lib.rs\nreadme.txt",
      "info": "total 2 entries"
    }
  },
  "executor": "local"
}
```

## `read` Binary File

Request:

```json
{"id":6,"tool":"read","params":{"filePath":"data/file.bin","mode":"binary","hashCheckMode":true},"directory":"/tmp/re-doc-sample"}
```

Response:

```json
{
  "id": 6,
  "ok": true,
  "result": {
    "metadata": {
      "file": {
        "canonicalPath": "/tmp/re-doc-sample/data/file.bin",
        "fileKey": "file-id:Inode { device_id: 138, inode_number: 412781 }",
        "kind": "file",
        "mtimeMs": 1780467082019,
        "size": 5
      },
      "hashCode": "sha256:08bb5e5d6eaac1049ede0893d30ed022b1a4d9b5b48db414871f51c9cb35283d"
    },
    "output": {
      "message": "",
      "text": "00000000  00 01 02 03 04                                   |.....|",
      "info": "total 5 bytes"
    }
  },
  "executor": "local"
}
```

## `stat`

Request:

```json
{"id":3,"tool":"stat","params":{"filePath":"missing.txt"},"directory":"/tmp/re-doc-sample"}
```

Response:

```json
{
  "id": 3,
  "ok": true,
  "result": {
    "metadata": {
      "file": {
        "canonicalPath": "/tmp/re-doc-sample/missing.txt",
        "fileKey": "missing:/tmp/re-doc-sample/missing.txt",
        "kind": "missing"
      }
    },
    "output": {
      "message": "",
      "text": "kind: missing\ncanonicalPath: /tmp/re-doc-sample/missing.txt\nfileKey: missing:/tmp/re-doc-sample/missing.txt",
      "info": ""
    }
  },
  "executor": "local"
}
```

