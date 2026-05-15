# JSON-RPC reference

`ptywright serve --stdio` exposes JSON-RPC 2.0 over stdin/stdout using newline-delimited JSON by default, or LSP-style `Content-Length` frames with `--framing lsp`. `ptywright serve --socket PATH` serves the same protocol over local IPC: Unix sockets on macOS/Linux and Windows named pipes for `\\.\pipe\...` paths.

This is the first automation protocol. It is intentionally separate from `ptywright run` so protocol responses never mix with raw terminal output.

## Framing rules

- stdout is protocol-only.
- stderr is diagnostics only.
- `--framing ndjson` default: stdin accepts one complete JSON-RPC request or notification per line; stdout writes one compact JSON-RPC response or notification per line.
- `--framing lsp`: messages are framed as `Content-Length: N\r\n\r\n<json>`.
- `--socket PATH`: local IPC transport. On macOS/Linux this is a Unix socket path. On Windows this is a named-pipe path such as `\\.\pipe\ptywright`. Each connection gets its own notification subscription state while sharing one server session registry, so multiple clients can inspect/control the same sessions.

## Example

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | \
  ptywright serve --stdio

printf 'Content-Length: 48\r\n\r\n{"jsonrpc":"2.0","id":1,"method":"session.list"}' | \
  ptywright serve --stdio --framing lsp
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

| Field                   | Required | Meaning                                                                    |
| ----------------------- | -------- | -------------------------------------------------------------------------- |
| `program`               | yes      | Executable name or path.                                                   |
| `args`                  | no       | Argument list.                                                             |
| `cwd`                   | no       | Working directory.                                                         |
| `env`                   | no       | Environment overrides.                                                     |
| `rows`/`cols`           | no       | Initial terminal size, default 24x80.                                      |
| `pixel_width`           | no       | Optional pixel width.                                                      |
| `pixel_height`          | no       | Optional pixel height.                                                     |
| `transcript_max_chars`  | no       | Bounded in-memory transcript retention; default is 128 KiB of UTF-8 chars. |
| `raw_transcript_path`   | no       | Explicit trusted-local path for raw/unredacted transcript byte streaming.  |
| `raw_transcript_append` | no       | Append to an existing raw transcript file; default refuses overwrites.     |

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

### `server.set_notifications`

Enable or disable coalesced server-originated notifications. Notifications are disabled by default so request/response clients continue to receive one response per request.

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "server.set_notifications",
  "params": { "enabled": true }
}
```

When enabled, ptywright may emit:

```json
{"jsonrpc":"2.0","method":"session.changed","params":{"session":"s1","sequence":3}}
{"jsonrpc":"2.0","method":"session.exited","params":{"session":"s1","sequence":3}}
```

For a request, the direct response is written first, followed by any queued notifications.

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
    "plain_text": "ready",
    "cells": [],
    "alternate_screen": false,
    "application_cursor": false,
    "application_keypad": false,
    "title": null
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
{"type":"screen_stable","value":{"min_ms":250}}
{"type":"process_exited"}
{"type":"any","value":[{"type":"contains_text","value":"ready"}]}
{"type":"all","value":[{"type":"contains_text","value":"rea"},{"type":"contains_text","value":"dy"}]}
```

### Claude Code convenience methods

Claude methods drive interactive Claude Code through a PTY. They do not use `claude -p`. The `claude.*` surface is backed by the built-in `claude-code` Lua plugin, while Rust executes PTY/session/action/matcher controls.

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

### Plugin methods

| Method                     | Params                    | Result                    |
| -------------------------- | ------------------------- | ------------------------- |
| `plugin.capabilities`      | none                      | Host plugin capabilities. |
| `plugin.validate_manifest` | `{ "manifest": { ... } }` | `{ "valid": true }`.      |

`plugin.capabilities` reports `embedded_lua: true` and includes built-in plugin manifests in `builtin_plugins`, including the `claude-code` Lua adapter. See [Plugins and extensions](./plugins.md) for manifest fields, runtime names, and permission names.

### `session.snapshot`

Return the latest parsed terminal screen for a session. Redacts sensitive-looking text by default.

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "session.snapshot",
  "params": { "session": "s1" }
}
```

Result is a `ScreenSnapshot` with the same shape as `session.wait`'s `snapshot` field (size, cursor, sequence, plain_text, cells, alternate_screen, application_cursor, application_keypad, title).

| Field       | Required | Meaning                                                                                                                                                                                                                                                                                                                    |
| ----------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `session`   | yes      | Session ID returned by `session.create`.                                                                                                                                                                                                                                                                                   |
| `redact`    | no       | Default `true`. Set `false` to opt into raw, unredacted screen text for trusted-local debugging.                                                                                                                                                                                                                           |
| `redaction` | no       | Optional `{ enabled, replacement, extra_literals, extra_regexes }` object adding caller-supplied redaction rules for this read only. The top-level `redact` flag controls whether redaction runs; the embedded `enabled` field must be present in JSON but is forced to `true` by the server when this object is supplied. |

### `session.transcript`

Return the in-memory transcript for a session. Bounded by `transcript_max_chars` (default 128 KiB of UTF-8 chars) at session creation. Redacts sensitive-looking text by default.

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "session.transcript",
  "params": { "session": "s1" }
}
```

Result:

```json
{ "text": "ready\n" }
```

Accepts the same `redact` and `redaction` params as `session.snapshot`.

### `session.resize`

Update the PTY's terminal size. Pixel dimensions are optional and default to `0`.

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "method": "session.resize",
  "params": { "session": "s1", "rows": 40, "cols": 120 }
}
```

Result:

```json
{ "resized": true }
```

| Field          | Required | Meaning                             |
| -------------- | -------- | ----------------------------------- |
| `session`      | yes      | Session ID.                         |
| `rows`         | yes      | New terminal rows.                  |
| `cols`         | yes      | New terminal cols.                  |
| `pixel_width`  | no       | Optional pixel width, default `0`.  |
| `pixel_height` | no       | Optional pixel height, default `0`. |

### `session.kill`

Kill the underlying child process. The session ID stays registered so callers can still read its final snapshot/transcript.

```json
{
  "jsonrpc": "2.0",
  "id": 9,
  "method": "session.kill",
  "params": { "session": "s1" }
}
```

Result:

```json
{ "killed": true }
```

### `session.close`

Kill the child process and remove the session ID from the server registry. Subsequent reads against the ID return `-32602` invalid params.

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "method": "session.close",
  "params": { "session": "s1" }
}
```

Result:

```json
{ "closed": true }
```

### Redaction notes

`session.snapshot` and `session.transcript` redact by default. Pass `"redact": false` for raw output, or `redaction: { ... }` to add per-call rules. See each method's params table.

`raw_transcript_path` streams raw PTY bytes directly to a file and is always explicit opt-in. The default mode creates a new file and refuses to overwrite; `raw_transcript_append: true` appends to an existing file. On Unix, ptywright creates new raw transcript files with mode `0o600` so only the owner can read them; appending to a pre-existing file preserves whatever permissions that file already has, and Windows uses default ACLs in either mode. Raw transcript files are unredacted sensitive data and remain the caller's responsibility to protect.

RPC error messages are redacted with the default policy before they are serialized. CLI-level diagnostics printed by ptywright also redact through the default policy.

## Notifications

The server accepts JSON-RPC notifications. Server-originated notifications are opt-in per connection through `server.set_notifications` and are currently emitted after request/notification handling rather than from a fully asynchronous event loop.

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
