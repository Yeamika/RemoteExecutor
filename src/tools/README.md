# Tool Interfaces

All tools return the same `ToolResult` shape on success:

```json
{
  "output": { "message": "", "text": "human-readable text for the model", "info": "" },
  "metadata": {}
}
```

Errors do not return `ToolResult`; the caller layer returns `ok: false` with an error string.

Per-tool README examples use the raw JSONL RPC shape. Request examples include `id`, `tool`, `params`, and optional `directory`. Response examples include `id`, `ok`, `result.metadata`, `result.output`, and `executor`.

Tool families:

| Tool family | Tools | Details |
|---|---|---|
| `fs` | `glob`, `read`, `stat` | `fs/README.md` |
| `file_action` | `FileAction` | `file_action/README.md` |
| `rg` | `rg` | `rg.md` |
| `exbash` | `exbash` with `mode` `run|shell|list|attach|stop|remove` | `exbash/README.md` |

General rules:

- `output` is optimized for direct model consumption.
- `metadata` is structured JSON for host code and tests.
- File mutation tools must not return full before/after file contents in `output` or `metadata`.
- `read` is the only file tool that intentionally returns file content, and it returns bounded slices.
- `hashCode` values are full SHA-256 strings in the form `sha256:<64 lowercase hex chars>`.
- Paths are resolved through `ToolContext.directory` unless they are absolute.

Small tools (`read`, `glob`, `stat`, `FileAction`, `rg`) are wrapped by the Executor host timeout. Exbash tools use their own PTY timeout behavior.