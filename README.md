# RemoteExecutor

Remote execution building blocks backed by the vendored `pty-t` submodule.

Modules:

- `Executor`: WS execution node for file tools, patch tools, and PTY-backed exbash tools.
- `Caller`: stdio/tool front door that manages multiple Executors.
- `ShellManager`: controlled PTY/session manager with a `ptyt`-compatible WebSocket endpoint.
- `fs_ops`: opencode-style `glob`, `grep`, and `read` helpers. Search is backed by ripgrep crates, not an external `rg` binary.
- `patch`: standard unified diff application via `diffy` plus opencode-style `apply_patch` support.

Run a standalone Executor node:

```bash
cargo run --bin remote-executor -- --id linux-box --listen 0.0.0.0:9001
```

The same `--listen` endpoint now accepts both Caller tool requests and `pty-t` clients.

Release packages include GNU Linux builds plus musl static Linux builds. Use the `*-musl-static` packages for older distributions such as Ubuntu 18 or minimal buildroot-style systems where newer glibc dependencies are a problem.

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

Run the MCP stdio wrapper for Caller:

```bash
cargo run --bin remote-caller-mcp
```

The Caller stdio bridge accepts requests like `{ "id": 1, "tool": "read", "params": { ... } }` and returns `{ "id": 1, "ok": true, "result": { ... } }`. The request field is `tool`; `method` is not accepted for Caller/Executor tool calls.
The MCP wrapper speaks JSON-RPC over stdio and exposes the same Caller/Executor tools through `tools/list` and `tools/call`.
Small tools (`read`, `glob`, `grep`, `apply_patch`, `diffy`, `rg`) have a host-side timeout: default `5000ms`, maximum `600000ms`, configurable with `toolTimeoutMs`. Exbash tools are handled separately through their own timeout fields and run on the same PTY backend as terminal sessions.
Stdio requests are handled concurrently in the same process. Write operations are not queued: if a write operation is already running, another write operation returns an error immediately.

Patch tools:

- `diffy`: applies standard unified/git diffs, for example `{ "id": 1, "tool": "diffy", "params": { "patchText": "--- a/file\n+++ b/file\n@@ ..." } }`.
- `apply_patch`: applies opencode's `*** Begin Patch` envelope format.

Caller tools:

- `list_executor`
- `connect_to_executor`
- `set_default_executor`

Exbash tools:

- `exbash`: start a command and read for `read_timeout` milliseconds before detaching; use `read_timeout: 0` to detach immediately. `timeout` is the optional total command runtime limit; omit it or set `timeout: -1` for no total limit.
- `exbash_list`: list runs.
- `exbash_attach`: write text or file input, wait until `read_timeout`, and return a PTY snapshot.
- `exbash_stop`: stop a run.
- `exbash_remove`: remove a stopped run.

Executors are addressed over WebSocket. The built-in `local` executor is started automatically by `Caller`.
MCP calls can route to a specific Executor with the optional `targetExecutor` argument.
Caller-to-Executor connection/response timeout is an internal fixed default of `30000ms` and is not exposed as a tool argument.

The WebSocket endpoint accepts terminal clients and read-only admin requests (`ptyt list`, `ptyt detail <pty>`). It rejects remote create/control/kill/listen/send operations.
Detached `exbash` runs are visible as PTY sessions on the same Executor WebSocket, so `ptyt`/`ptyc` clients can list and attach to them by `asyncID`.
`exbash_attach` waits until its `read_timeout` elapses, then returns the current PTY window snapshot as plain text in `output`. Metadata keeps `wrote`, `source`, and `outputBytes`, where `outputBytes` is the number of PTY output bytes captured after attach started. If the task already stopped, attach returns the final snapshot immediately and sets `message`, `state`, `exitCode`, and `inputFailed`; when input was requested, `message` starts with `input failed`. It does not write log files or accept a tail-size argument.
