# Exbash Tool

Tool name: `exbash`

`exbash` is PTY-backed and uses `mode` to select the operation. It is not wrapped by the small-tool timeout. Run modes use `timeout` for total process lifetime and `read_timeout` for how long to wait before returning a snapshot.

## Input

```json
{
  "mode": "run",
  "command": "echo hello",
  "description": "optional display text",
  "timeout": 30000,
  "read_timeout": 1000,
  "asyncID": "rex-...",
  "text": "input to write",
  "filePath": "input.txt",
  "workdir": "subdir",
  "showRawPretty": false,
  "shell": "bash"
}
```

Modes:

| `mode` | Required fields | Behavior |
|---|---|---|
| `run` | `command` | Runs `command` directly, without shell wrapping. |
| `shell` | `command` | Runs `command` through a configured shell profile. |
| `attach` | `asyncID` | Writes optional input and returns a PTY snapshot. |
| `list` | none | Lists exbash runs. `asyncID` may filter to one run. |
| `stop` | `asyncID` | Stops a run and returns the stopped snapshot. |
| `remove` | `asyncID` | Removes a run, stopping it first if needed. |

Common fields:

| Field | Notes |
|---|---|
| `command` | Command input, limited to 4096 bytes. |
| `description` | Optional display text, limited to 100 bytes. |
| `timeout` | Total runtime in ms. Omit, `0`, or `-1` means no total timeout. |
| `read_timeout` | Read wait in ms. Defaults to `10000`. |
| `asyncID` | Run id for `list`, `attach`, `stop`, and `remove`. |
| `text` | Text input for `attach`, limited to 4096 bytes after escape parsing. |
| `filePath` | File input for `attach`, limited to 4096 bytes. |
| `workdir` | Optional cwd for `run` and `shell`. Relative paths resolve against the top-level RPC `directory`. |
| `showRawPretty` | Adds `rawPretty` to attach metadata. |
| `shell` | Shell profile for `mode: "shell"`. Empty or omitted uses settings default. |

## Mode `run`

Completed request:

```json
{
  "id": 20,
  "tool": "exbash",
  "params": {
    "mode": "run",
    "command": "printf hello",
    "read_timeout": 1000
  }
}
```

Completed response:

```json
{
  "id": 20,
  "ok": true,
  "result": {
    "metadata": {
      "exitCode": 0,
      "output": "hello"
    },
    "output": { "message": "", "text": "hello", "info": "" }
  },
  "executor": "local"
}
```

Detached request:

```json
{
  "id": 30,
  "tool": "exbash",
  "params": {
    "mode": "run",
    "command": "bash -lc 'printf detached-text; sleep 10'",
    "read_timeout": 200
  }
}
```

Detached response:

```json
{
  "id": 30,
  "ok": true,
  "result": {
    "metadata": {
      "asyncID": "rex-1780467386041-1",
      "command": "bash -lc 'printf detached-text; sleep 10'",
      "cwd": "/runtime/workspaces/maintainer/RemoteExecutor",
      "description": "bash -lc 'printf detached-text; sleep 10'",
      "detached": true,
      "pid": 57687,
      "startedAt": 1780467386042,
      "state": "running",
      "timeout": null,
      "totalOutput": 13
    },
    "output": { "message": "rex-1780467386041-1 detached", "text": "detached-text", "info": "" }
  },
  "executor": "local"
}
```

## Mode `shell`

Request:

```json
{
  "id": 40,
  "tool": "exbash",
  "params": {
    "mode": "shell",
    "command": "printf shell",
    "read_timeout": 1000
  }
}
```

Response:

```json
{
  "id": 40,
  "ok": true,
  "result": {
    "metadata": {
      "exitCode": 0,
      "output": "shell"
    },
    "output": { "message": "", "text": "shell", "info": "" }
  },
  "executor": "local"
}
```

## Mode `attach`

Request:

```json
{
  "id": 24,
  "tool": "exbash",
  "params": {
    "mode": "attach",
    "asyncID": "rex-1780467225438-2",
    "text": "doc input\\n",
    "read_timeout": 100
  }
}
```

Response:

```json
{
  "id": 24,
  "ok": true,
  "result": {
    "metadata": {
      "asyncID": "rex-1780467225438-2",
      "outputBytes": 22,
      "source": "text",
      "wrote": 10
    },
    "output": { "message": "", "text": "doc input\ndoc input", "info": "" }
  },
  "executor": "local"
}
```

Stopped attach responses use the same JSON wrapper and include `state` and `exitCode` in `result.metadata`; the status text is in `result.output.message`.

## Mode `list`

Request:

```json
{
  "id": 22,
  "tool": "exbash",
  "params": {
    "mode": "list"
  }
}
```

Response:

```json
{
  "id": 22,
  "ok": true,
  "result": {
    "metadata": {
      "runs": [
        {
          "asyncID": "rex-1780467225438-2",
          "command": "bash -lc read line; echo $line; sleep 5",
          "cwd": "/runtime/workspaces/maintainer/RemoteExecutor",
          "description": "bash -lc read line; echo $line; sleep 5",
          "pid": 55303,
          "startedAt": 1780467225440,
          "state": "running",
          "timeout": null,
          "totalOutput": 0
        }
      ]
    },
    "output": { "message": "", "text": "rex-1780467225438-2 running totalOutput=0 command=bash -lc read line; echo $line; sleep 5", "info": "" }
  },
  "executor": "local"
}
```

Stopped runs include `exitCode` and `endedAt`. `error` is omitted unless an error is recorded.

## Mode `stop`

Request:

```json
{
  "id": 31,
  "tool": "exbash",
  "params": {
    "mode": "stop",
    "asyncID": "rex-1780467386041-1"
  }
}
```

Response:

```json
{
  "id": 31,
  "ok": true,
  "result": {
    "metadata": {
      "asyncID": "rex-1780467386041-1",
      "command": "sleep 10",
      "cwd": "/runtime/workspaces/maintainer/RemoteExecutor",
      "description": "sleep 10",
      "endedAt": 1780467404893,
      "exitCode": "stopped",
      "pid": 57687,
      "startedAt": 1780467386042,
      "state": "stopped",
      "timeout": null,
      "totalOutput": 0
    },
    "output": { "message": "", "text": "", "info": "" }
  },
  "executor": "local"
}
```

## Mode `remove`

Request:

```json
{
  "id": 26,
  "tool": "exbash",
  "params": {
    "mode": "remove",
    "asyncID": "rex-1780467225438-2"
  }
}
```

Response:

```json
{
  "id": 26,
  "ok": true,
  "result": {
    "metadata": {
      "ok": true
    },
    "output": { "message": "", "text": "ok", "info": "" }
  },
  "executor": "local"
}
```