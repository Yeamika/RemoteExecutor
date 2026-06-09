# RemoteExecutor

Remote execution building blocks backed by the vendored `pty-t` submodule.

Source layout:

- `protocol.rs`: Executor request/response types and `ToolResult`.
- `context.rs`: per-call `ToolContext`.
- `executor/`: Executor API, dispatch, shared WebSocket endpoint, and adjacent tests.
- `caller/`: Caller API, stdio/MCP front doors, executor state, and adjacent tests.
- `tools/`: tool implementations for `fs`, `file_action`, `rg`, and `exbash`.
- `settings/`: `.re-setting.json` loading, hot reload, and shell profile resolution.
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
cargo run --bin remote-caller -- --settings .re-setting.json
```

Run the MCP stdio wrapper for Caller:

```bash
cargo run --bin remote-caller-mcp -- --settings .re-setting.json
```

The Caller stdio bridge accepts requests like `{ "id": 1, "tool": "read", "params": { ... } }` and returns `{ "id": 1, "ok": true, "result": { ... } }`. The request field is `tool`; `method` is not accepted for Caller/Executor tool calls.
The MCP wrapper speaks JSON-RPC over stdio and exposes the same Caller/Executor tools through `tools/list` and `tools/call`.
Small tools (`read`, `glob`, `stat`, `FileAction`, `rg`) have a host-side timeout: default `5000ms`, maximum `600000ms`, configurable with `toolTimeoutMs`. Exbash tools are handled separately through their own timeout fields and run on the same PTY backend as terminal sessions.
Stdio requests are handled concurrently in the same process. Write operations are not queued: if a write operation is already running, another write operation returns an error immediately.

File actions:

- `FileAction`: creates, deletes, renames, or patches a single file. Parameters: `mode`, `filePath`, and mode-specific fields. When `hashCheckMode` is true, the current file hash must match `hashCode`, and mutating actions that leave a file return the new full `hashCode`.

Patch mode uses `mode: "patch"`, `patchText`, optional `patchMode` (`text` by default, or `binary`), optional `hashCheckMode`, and optional `hashCode`. Text patch syntax uses 1-based line numbers from the file snapshot at the start of the patch, with one instruction per line: `***DELETE*** start-end`, `***MOVE*** start-end,startline` to move a block after `startline`, `***APPEND_HEAD*** startline` followed by literal lines and `***APPEND_END***`, and `n:new line text` to replace line `n`. Use startline `0` to insert at the start and `-1` to append at the end. Example: `{ "tool": "FileAction", "params": { "mode": "patch", "filePath": "src/foo.rs", "patchText": "11:new line\n***APPEND_HEAD*** 11\nextra\n***APPEND_END***", "hashCheckMode": true, "hashCode": "sha256:..." } }`.

Binary patch mode uses original 0-based byte offsets with `patchMode: "binary"`. Hunk headers are `replace OFFSET LEN`, `delete OFFSET LEN`, and `insert OFFSET`; `insert 0` inserts at the start, `insert -1` inserts at the end, and positive `insert OFFSET` inserts at that byte offset. Body lines are `+HEX`, and multiple body lines concatenate. Example: `{ "tool": "FileAction", "params": { "mode": "patch", "filePath": "data.bin", "patchMode": "binary", "patchText": "replace 10 2\n+AA BB", "hashCheckMode": true, "hashCode": "sha256:..." } }`.

Create/delete/rename use `mode: "create"` with `content`, `mode: "delete"`, and `mode: "rename"` with `newFilePath` respectively.

Caller tools:

- `list_executor`
- `connect_to_executor`
- `set_default_executor`

- `set_default_shell`
Exbash tool:

- `exbash`: PTY-backed command control through `mode`.
- `mode: "run"`: start a command directly and read for `read_timeout` milliseconds before detaching.
- `mode: "shell"`: start a command through the configured shell profile. Pass `shell` to use a specific profile; omit it or pass an empty string to use `shells.default`.
- `mode: "list"`: list runs.
- `mode: "attach"`: write text or file input, wait until `read_timeout`, and return a PTY snapshot.
- `mode: "stop"`: stop a run.
- `mode: "remove"`: stop a running run if needed, close connected PTY clients, and remove the run.

Shell settings:

Programs load `.re-setting.json` from the startup directory by default; `--settings <path>` overrides that path. Settings are hot-reloaded when the file `mtime` or size changes. The shell section supports `default`, `interactive`, and named `profiles`; built-in profiles are `bash`, `python`, `node`, and `powershell`. Candidate paths are checked in order, relative candidates are resolved from the settings file directory, and bare names are resolved through `PATH`. `set_default_shell` updates `shells.default` on the target Executor and writes the settings file back.

```json
{
  "version": 1,
  "shells": {
    "default": "auto",
    "interactive": "auto",
    "profiles": {
      "python": {
        "candidates": [".venv/bin/python", ".venv/Scripts/python.exe", "python3", "python"],
        "commandArgs": ["-c", "{command}"],
        "interactiveArgs": []
      }
    }
  }
}
```

Executors are addressed over WebSocket. The built-in `local` executor is started automatically by `Caller`.
MCP calls can route to a specific Executor with the optional `targetExecutor` argument.
Caller-to-Executor connection/response timeout is an internal fixed default of `30000ms` and is not exposed as a tool argument.

The WebSocket endpoint accepts terminal clients and read-only admin requests (`ptyt list`, `ptyt detail <pty>`). It rejects remote create/control/kill/listen/send operations.
Detached `exbash` runs are visible as PTY sessions on the same Executor WebSocket, so `ptyt`/`ptyc` clients can list and attach to them by `asyncID`.
`exbash` mode `attach` waits until its `read_timeout` elapses, then returns the current PTY window snapshot in `output.text`. Metadata keeps `wrote`, `source`, and `outputBytes`, where `outputBytes` is the number of PTY output bytes captured after attach started. If `showRawPretty` is true, attach also includes `rawPretty` in metadata; it defaults to false. When attach sends input, it takes PTY controller as `rec:<asyncID>` and leaves that controller in place; if a ptyt/ptyc client takes control before `read_timeout`, attach fails immediately with `control lost: someone attached: <client-id>`. If the task already stopped, attach returns the final snapshot immediately, sets `state` and `exitCode` in metadata, and puts the status text in `output.message`; when input was requested, `output.message` starts with `input failed`. Mode `stop` also returns a plain text snapshot in `output.text`; mode `remove` returns `ok` in `output.text` with `metadata.ok = true`. It does not write log files or accept a tail-size argument.
When RE kills a run because of total `timeout` or mode `stop`, `exitCode` is the string `"timeout"` or `"stopped"`; normal process exits still use numeric exit codes.
`exbash` inputs are intentionally small: `command`, `filePath`, attach `text` (stdin content), and attach file contents are limited to 4096 bytes; `description` is limited to 100 bytes and `asyncID` is limited to 30 bytes. Oversized inputs are rejected.
