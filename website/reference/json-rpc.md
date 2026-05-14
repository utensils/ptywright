# JSON-RPC reference

`ptywright serve --stdio` exposes JSON-RPC 2.0 over newline-delimited JSON.

This is the first automation protocol. It is intentionally separate from `ptywright run` so protocol responses never mix with raw terminal output.

## Framing rules

- stdin: one complete JSON-RPC request or notification per line.
- stdout: one compact JSON-RPC response per line.
- stderr: diagnostics only.
- Current framing: NDJSON.
- Planned framing: optional LSP-style `Content-Length` framing.

## Example

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | \
  ptywright serve --stdio
```

## Methods

### `server.capabilities`

Returns protocol metadata, available methods, and notification support.

```json
{ "jsonrpc": "2.0", "id": 1, "method": "server.capabilities" }
```

### `session.create`

Spawn a PTY-backed session.

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session.create",
  "params": {
    "program": "/bin/sh",
    "args": ["-lc", "printf ready"],
    "rows": 24,
    "cols": 80
  }
}
```

Result:

```json
{ "session": "s1" }
```

Params:

| Field          | Required | Meaning                               |
| -------------- | -------- | ------------------------------------- |
| `program`      | yes      | Executable name or path.              |
| `args`         | no       | Argument list.                        |
| `cwd`          | no       | Working directory.                    |
| `env`          | no       | Environment overrides.                |
| `rows`/`cols`  | no       | Initial terminal size, default 24x80. |
| `pixel_width`  | no       | Optional pixel width.                 |
| `pixel_height` | no       | Optional pixel height.                |

### `session.list`

Return active session IDs.

```json
{ "jsonrpc": "2.0", "id": 3, "method": "session.list" }
```

### `session.input`

Send an action to a session.

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "session.input",
  "params": { "session": "s1", "action": { "type": "text", "value": "help\n" } }
}
```

Action payloads use `type` plus `value` where needed:

```json
{"type":"text","value":"hello"}
{"type":"paste","value":"multi\nline"}
{"type":"key","value":"enter"}
{"type":"resize","value":{"rows":40,"cols":120,"pixel_width":0,"pixel_height":0}}
{"type":"interrupt"}
{"type":"eof"}
{"type":"kill"}
```

### `session.wait`

Wait for a matcher to succeed.

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "session.wait",
  "params": {
    "session": "s1",
    "matcher": { "type": "contains_text", "value": "ready" },
    "timeout_ms": 5000
  }
}
```

Result includes evidence:

```json
{
  "matched": true,
  "sequence": 1,
  "elapsed_ms": 12,
  "snapshot": {
    "size": { "rows": 24, "cols": 80, "pixel_width": 0, "pixel_height": 0 },
    "cursor": { "row": 0, "col": 5, "visible": true },
    "sequence": 1,
    "plain_text": "ready"
  },
  "transcript_tail": "ready"
}
```

Matcher payloads:

```json
{"type":"contains_text","value":"ready"}
{"type":"screen_regex","value":"rea.y"}
{"type":"transcript_contains","value":"ready"}
{"type":"transcript_regex","value":"rea.y"}
{"type":"cursor_at","value":{"row":0,"col":0}}
{"type":"any","value":[{"type":"contains_text","value":"ready"}]}
{"type":"all","value":[{"type":"contains_text","value":"rea"},{"type":"contains_text","value":"dy"}]}
```

### Claude Code convenience methods

Claude methods drive interactive Claude Code through a PTY. They do not use `claude -p`.

| Method               | Params                                        | Result                             |
| -------------------- | --------------------------------------------- | ---------------------------------- |
| `claude.start`       | `{ "cwd": "/repo", "rows": 40, "cols": 120 }` | `{ "claude": "c1", "state": ... }` |
| `claude.send_prompt` | `{ "claude": "c1", "prompt": "..." }`         | `{ "state": ... }`                 |
| `claude.wait_turn`   | `{ "claude": "c1", "timeout_ms": 120000 }`    | `{ "state": ... }`                 |
| `claude.approve`     | `{ "claude": "c1" }`                          | `{ "approved": true }`             |
| `claude.deny`        | `{ "claude": "c1" }`                          | `{ "denied": true }`               |
| `claude.cancel`      | `{ "claude": "c1" }`                          | `{ "state": ... }`                 |
| `claude.state`       | `{ "claude": "c1" }`                          | `{ "state": ... }`                 |

`claude.start` accepts optional `program`, `args`, `cwd`, `env`, `rows`, `cols`, `pixel_width`, and `pixel_height` fields. The default program is `claude`; default args are empty.

See the [Claude Code adapter guide](../guide/claude-code.md) for state semantics and limitations.

### Other session methods

| Method               | Params                                         | Result                         |
| -------------------- | ---------------------------------------------- | ------------------------------ |
| `session.snapshot`   | `{ "session": "s1" }`                          | Current `ScreenSnapshot`.      |
| `session.transcript` | `{ "session": "s1" }`                          | `{ "text": "..." }`.           |
| `session.resize`     | `{ "session": "s1", "rows": 40, "cols": 120 }` | `{ "resized": true }`.         |
| `session.kill`       | `{ "session": "s1" }`                          | `{ "killed": true }`.          |
| `session.close`      | `{ "session": "s1" }`                          | Kills and removes the session. |

## Notifications

The server accepts JSON-RPC notifications, but it does not emit asynchronous notifications yet. Coalesced `session.output`, `screen.changed`, and `session.exited` notifications are planned once the event subscription model lands.

## Error codes

| Code     | Meaning           |
| -------- | ----------------- |
| `-32700` | Parse error.      |
| `-32600` | Invalid request.  |
| `-32601` | Method not found. |
| `-32602` | Invalid params.   |
| `-32603` | Internal error.   |
| `-32001` | Matcher timeout.  |
| `-32002` | Session closed.   |
