# RG Tool

Tool name: `rg`

Request used for the captured example:

```json
{
  "id": 5,
  "tool": "rg",
  "params": {
    "pattern": "fn main",
    "root": "/tmp/re-doc-sample",
    "path": "src"
  },
  "directory": "/tmp/re-doc-sample"
}
```

Field notes:

| Field | Notes |
|---|---|
| `pattern` | Required regex pattern. |
| `root` | Optional root directory. Defaults to current directory. |
| `path` | Optional file or directory under `root`. |
| `globs` | Optional include glob filters. |
| `case_sensitive` | Optional. Defaults to true. |
| `max_count` | Optional maximum number of matches. |

Response captured from the request above:

```json
{
  "id": 5,
  "ok": true,
  "result": {
    "metadata": {
      "code": 0,
      "matches": 1
    },
    "output": { "message": "", "text": "/tmp/re-doc-sample/src/lib.rs:1:1:fn main() {}\n", "info": "" }
  },
  "executor": "local"
}
```

`code` is the process-style result code from the search implementation. A no-match result can have a non-zero code while still returning a successful tool result.