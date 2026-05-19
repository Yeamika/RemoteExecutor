# RemoteExecutor

Remote execution building blocks backed by the vendored `pty-t` submodule.

Modules:

- `ShellManager`: controlled PTY/session manager with a `ptyt`-compatible WebSocket endpoint.
- `fs_ops`: opencode-style `glob`, `grep`, and `read` helpers. Search is backed by ripgrep crates, not an external `rg` binary.
- `patch`: opencode-style `apply_patch` support.
- `exbash`: opencode-style async shell execution.
- `remote-executor-stdio`: JSONL bridge for tool-style requests.

Run the demo:

```bash
cargo run --manifest-path RemoteExecutor/Cargo.toml --bin remote-executor-demo
```

The stdio bridge accepts requests like `{ "id": 1, "tool": "read", "params": { ... } }` and returns `{ "id": 1, "ok": true, "result": { ... } }`.

The WebSocket endpoint accepts terminal clients and read-only admin requests (`ptyt list`, `ptyt detail <pty>`). It rejects remote create/control/kill/listen/send operations.
