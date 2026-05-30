# RemoteExecutor

Remote execution building blocks backed by the vendored `pty-t` submodule.

Modules:

- `Executor`: WS execution node for file tools, patch tools, and PTY-backed exbash tools.
- `Caller`: stdio/tool front door that manages multiple Executors.
- `ShellManager`: controlled PTY/session manager with a `ptyt`-compatible WebSocket endpoint.
- `fs_ops`: opencode-style `glob`, `grep`, and `read` helpers. Search is backed by ripgrep crates, not an external `rg` binary.
- `patch`: single-file line-number patch support through `apply_patch`.

Run a standalone Executor node:

```bash
cargo run --bin remote-executor -- --id linux-box --listen 0.0.0.0:9001
```

The same `--listen` endpoint now accepts both Caller tool requests and `pty-t` clients.

Release packages include GNU Linux builds plus musl static Linux builds. Use the `*-musl-static` packages for older distributions such as Ubuntu 18 or minimal buildroot-style systems where newer glibc dependencies are a problem. For 32-bit systems, use `remote-executor-linux-i686-musl-static` on x86 and try `remote-executor-linux-armv7-musl-static` first on ARM SoC boards.

`read` and `stat` return `metadata.file` as a FileStamp: `fileKey`, `canonicalPath`, `kind`, and optional `size`/`mtimeMs`. With `hashCheckMode: true`, `read` also returns `hashCode` as a full SHA-256 digest for the file bytes.

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
Small tools (`read`, `glob`, `grep`, `apply_patch`, `rg`) have a host-side timeout: default `5000ms`, maximum `600000ms`, configurable with `toolTimeoutMs`. Exbash tools are handled separately through their own timeout fields and run on the same PTY backend as terminal sessions.
Stdio requests are handled concurrently in the same process. Write operations are not queued: if a write operation is already running, another write operation returns an error immediately.

Patch tools:

- `apply_patch`: applies a single-file patch. Parameters: `filePath`, `patchText`, optional `patchMode` (`text` by default, or `binary`), optional `hashCheckMode`, optional `hashCode`. When `hashCheckMode` is true, the current file hash must match `hashCode`, and the result returns the new full `hashCode`.

Text patch syntax uses original 1-based line numbers. Hunk headers are `replace A B`, `delete A B`, and `insert N`; `insert 0` inserts at the start, `insert -1` inserts at the end, and `insert N` for positive N inserts after original line N. Body lines are `+text` for new text and `copy A B` to reuse original lines. Example: `{ "tool": "apply_patch", "params": { "filePath": "src/foo.rs", "patchText": "replace 11 11\n+new line", "hashCheckMode": true, "hashCode": "sha256:..." } }`.

Binary patch syntax uses original 0-based byte offsets with `patchMode: "binary"`. Hunk headers are `replace OFFSET LEN`, `delete OFFSET LEN`, and `insert OFFSET`; `insert 0` inserts at the start, `insert -1` inserts at the end, and positive `insert OFFSET` inserts at that byte offset. Body lines are `+HEX`, and multiple body lines concatenate. Example: `{ "tool": "apply_patch", "params": { "filePath": "data.bin", "patchMode": "binary", "patchText": "replace 10 2\n+AA BB", "hashCheckMode": true, "hashCode": "sha256:..." } }`.

Caller tools:

- `list_executor`
- `connect_to_executor`
- `set_default_executor`

Exbash tools:

- `exbash`: start a command directly, without shell wrapping, and read for `read_timeout` milliseconds before detaching; use `read_timeout: 0` to detach immediately. `timeout` is the optional total command runtime limit; omit it or set `timeout: -1`/`0` for no total limit.
- `exbash_shell`: start a command through the platform shell (`sh -c` on Unix, `powershell.exe -Command` on Windows) with the same timeout behavior as `exbash`.
- `exbash_list`: list runs.
- `exbash_attach`: write text or file input, wait until `read_timeout`, and return a PTY snapshot.
- `exbash_stop`: stop a run.
- `exbash_remove`: stop a running run if needed, close connected PTY clients, and remove the run.

Executors are addressed over WebSocket. The built-in `local` executor is started automatically by `Caller`.
MCP calls can route to a specific Executor with the optional `targetExecutor` argument.
Caller-to-Executor connection/response timeout is an internal fixed default of `30000ms` and is not exposed as a tool argument.

The WebSocket endpoint accepts terminal clients and read-only admin requests (`ptyt list`, `ptyt detail <pty>`). It rejects remote create/control/kill/listen/send operations.
Detached `exbash` runs are visible as PTY sessions on the same Executor WebSocket, so `ptyt`/`ptyc` clients can list and attach to them by `asyncID`.
`exbash_attach` waits until its `read_timeout` elapses, then returns the current PTY window snapshot as plain text in `output`. Metadata keeps `wrote`, `source`, and `outputBytes`, where `outputBytes` is the number of PTY output bytes captured after attach started. If `showRawPretty` is true, attach also includes `rawPretty` in metadata; it defaults to false. When attach sends input, it takes PTY controller as `rec:<asyncID>` and leaves that controller in place; if a ptyt/ptyc client takes control before `read_timeout`, attach fails immediately with `control lost: someone attached: <client-id>`. If the task already stopped, attach returns the final snapshot immediately and sets `message`, `state`, `exitCode`, and `inputFailed`; when input was requested, `message` starts with `input failed`. `exbash_stop` also returns a plain text snapshot in `output`; `exbash_remove` returns no output and marks `metadata.stopped` when it had to stop a running process before removal. It does not write log files or accept a tail-size argument.
When RE kills a run because of total `timeout` or `exbash_stop`, `exitCode` is the string `"timeout"` or `"stopped"`; normal process exits still use numeric exit codes.
`exbash` inputs are intentionally small: `command`, `filePath`, attach `text` (stdin content), and attach file contents are limited to 4096 bytes; `description` is limited to 100 bytes and `asyncID` is limited to 30 bytes. Oversized inputs are rejected.
