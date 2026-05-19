# RemoteExecutor

Remote execution building blocks backed by the vendored `pty-t` submodule.

Modules:

- `Executor`: WS execution node for `read`, `glob`, `grep`, `apply_patch`, `diffy`, and `exbash`.
- `Caller`: stdio/tool front door that manages multiple Executors.
- `ShellManager`: controlled PTY/session manager with a `ptyt`-compatible WebSocket endpoint.
- `fs_ops`: opencode-style `glob`, `grep`, and `read` helpers. Search is backed by ripgrep crates, not an external `rg` binary.
- `patch`: standard unified diff application via `diffy` plus opencode-style `apply_patch` support.

Run a standalone Executor node:

```bash
cargo run --bin remote-executor -- --id linux-box --listen 0.0.0.0:9001
```

The same `--listen` endpoint now accepts both Caller tool requests and `pty-t` clients.

```bash
cargo run --bin remote-executor -- --id linux-box --listen 0.0.0.0:9001 --pty main
```

Then connect with `pty-t`'s client on the same URL:

```bash
ptyt --url ws://host:9001 --pty main
```

Run a Caller for the upper tool layer:

```bash
cargo run --bin remote-caller
```

The Caller stdio bridge accepts requests like `{ "id": 1, "tool": "read", "params": { ... } }` and returns `{ "id": 1, "ok": true, "result": { ... } }`.

Patch tools:

- `diffy`: applies standard unified/git diffs, for example `{ "id": 1, "tool": "diffy", "params": { "patchText": "--- a/file\n+++ b/file\n@@ ..." } }`.
- `apply_patch`: applies opencode's `*** Begin Patch` envelope format.

Caller tools:

- `list_executor`
- `connect_to_executor`
- `set_default_executor`

Executors are addressed over WebSocket. The built-in `local` executor is started automatically by `Caller`.

The WebSocket endpoint accepts terminal clients and read-only admin requests (`ptyt list`, `ptyt detail <pty>`). It rejects remote create/control/kill/listen/send operations.
