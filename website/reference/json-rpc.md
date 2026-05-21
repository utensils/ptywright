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
{"type":"bracketed_paste","value":"wrap me in CSI 200~ ... CSI 201~"}
{"type":"key","value":"enter"}
{"type":"resize","value":{"rows":40,"cols":120,"pixel_width":0,"pixel_height":0}}
{"type":"interrupt"}
{"type":"eof"}
{"type":"kill"}
```

`paste` writes the bytes verbatim — use it against programs that have not enabled bracketed paste (cat, plain shells, generic REPLs). `bracketed_paste` wraps the payload in the standard bracketed-paste markers (`CSI 200 ~` … `CSI 201 ~`) — use it against TUIs that have enabled bracketed paste (Claude Code v2.1+, vim, fish, …) so a subsequent Enter is interpreted as a submit rather than absorbed into the paste tokeniser.

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

Optional `adapters` and `sessions` arrays scope the stream — events only fire for sessions whose id matches `sessions` or whose owning adapter id matches `adapters`. An empty / omitted filter delivers every session (the default). Adapter ids that don't match a live adapter at filter time are silently ignored so callers can pre-subscribe before `adapter.start` returns; the server response echoes the resolved filter:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "server.set_notifications",
  "params": { "enabled": true, "adapters": ["e1"], "sessions": ["s2"] }
}
```

```json
{ "enabled": true, "adapters": ["e1"], "sessions": ["s2"] }
```

When enabled, ptywright may emit:

```json
{"jsonrpc":"2.0","method":"session.changed","params":{"session":"s1","sequence":3}}
{"jsonrpc":"2.0","method":"session.output","params":{"session":"s1","sequence":4,"output":"hello\r\n"}}
{"jsonrpc":"2.0","method":"session.exited","params":{"session":"s1","sequence":4}}
```

`session.changed` fires when the parsed screen advances; `session.output` carries the transcript delta since the last poll (the `sequence` is the current value at poll time and may be an upper bound when multiple writes batched); `session.exited` fires once when the underlying child process terminates. On the off → on transition the server seeds each session's output cursor at its current position so a freshly-subscribed client only sees output produced _after_ subscription, not the retained transcript buffer. A `dropped: true` field is added to `session.output` if the per-tick delta was truncated against the transcript bound.

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
{"type":"lua","value":{"plugin":"<plugin-name>","predicate":"<exported-function>","params":{"anchor":"ready"}}}
```

`Matcher::Lua` defers to the bound plugin registry — the server holds a name → `LuaPlugin` map populated from built-in plugins and any trusted-local third-party plugins loaded via `--plugin` / `plugin.load`. Use the plugin's manifest `name` and any function it exports as a predicate (a function that returns a boolean or `{ matched, evidence?, capture? }` table — distinct from the `wait_*_matcher` functions that compose `Matcher` trees). Inspect `plugin.describe` to discover what a plugin exports.

The predicate function receives an input table with `screen`, `transcript`, `sequence`, `stable_ms`, `process_exited`, `markers` (transcript marker map), `cursor` (current transcript cursor), and `params` (the JSON forwarded from the matcher). Predicates return either a boolean shorthand or `{ matched, evidence?, capture? }`. A truthy outcome surfaces on the `adapter.wait` response as a `MatchOutcome::Lua { kind: "lua", plugin, predicate, evidence?, capture? }` branch.

The registry's plugin instances are SEPARATE from any adapter's own runtime instance — predicates cannot read module-level Lua state set by a classifier or intent on the adapter-side instance. v1 limitation; predicates that need shared state must pass it through the `params` table. See the Library reference for the Rust API (`PluginRegistry`, `PredicateContext`, `PredicateOutcome`, `LuaPluginRegistry`).

### Adapter methods

`adapter.*` is the generic plugin-name-aware surface for driving any TUI through ptywright's extension layer. Callers select a plugin manifest by name; the server spawns a PTY session and wraps it in an `ExtensionHandle` for that plugin. Subsequent calls reference the handle by id. There is no per-application RPC namespace — application-specific behaviour (state names, intent names, evidence strings) lives entirely in Lua plugins under `plugins/<name>/`.

| Method                | Params                                                                                                                               | Result                                                                                                                                                                                                                                              |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `adapter.list`        | —                                                                                                                                    | `{ "plugins": [PluginManifest, ...] }`                                                                                                                                                                                                              |
| `adapter.live`        | —                                                                                                                                    | `{ "adapters": [{ "adapter", "plugin", "session", "sequence", "finished" }, ...] }` — every adapter currently registered in shared state, sorted by id. Useful for re-connecting clients discovering adapters left running by a prior REPL session. |
| `adapter.start`       | `{ "plugin": "claude-code", "program"?, "args"?, "cwd"?, "env"?, "rows"?, "cols"?, "pixel_width"?, "pixel_height"? }`                | `{ "adapter": "e1", "plugin": "claude-code", "session": "s1", "state": ... }` (the `session` field is the host-allocated id paired with this adapter; drive PTY operations through the `adapter.*` surface, not the unrelated `session.*` methods)  |
| `adapter.resume`      | same as `adapter.start` plus `"prior_adapter"?`                                                                                      | same as `adapter.start`; closes `prior_adapter` first when it is supplied and still live                                                                                                                                                            |
| `adapter.state`       | `{ "adapter": "e1" }`                                                                                                                | `{ "state": ... }`                                                                                                                                                                                                                                  |
| `adapter.send`        | `{ "adapter": "e1", "intent": "send_prompt", "params": { "prompt": "..." } }`                                                        | `{ "state": ... }`                                                                                                                                                                                                                                  |
| `adapter.wait`        | `{ "adapter": "e1", "intent"?: "wait_turn_matcher", "params"?: { ... }, "timeout_ms"?: 120000, "wait_id"?: "..." }`                  | `{ "state": ..., "matched": { "kind": ..., "pattern"?, "capture"?, "text"?, "row"?, "col"?, "min_ms"?, "matched"? }, "wait_id"? }` — `matched` describes which matcher branch fired; `wait_id` is echoed when supplied                              |
| `adapter.turn`        | `{ "adapter": "e1", "send": { "intent": "...", "params"?: { ... } }, "wait"?: { "intent"?, "params"?, "timeout_ms"?, "wait_id"? } }` | `{ "state": ..., "matched": ..., "wait_id"? }` — atomic send-then-wait                                                                                                                                                                              |
| `adapter.cancel_wait` | `{ "wait_id": "..." }`                                                                                                               | `{ "cancelled": bool, "adapter"?, "wait_id"? }` — idempotent; unknown id returns `cancelled: false`                                                                                                                                                 |
| `adapter.snapshot`    | `{ "adapter": "e1", "redact"?, "redaction"? }`                                                                                       | `ScreenSnapshot` (same shape as `session.snapshot`)                                                                                                                                                                                                 |
| `adapter.transcript`  | `{ "adapter": "e1", "redact"?, "redaction"? }`                                                                                       | `{ "text": "..." }`                                                                                                                                                                                                                                 |
| `adapter.inspect`     | `{ "adapter": "e1", "redact"?, "redaction"? }`                                                                                       | `{ "adapter", "plugin", "state", "plain_text", "body_text", "status_text", "transcript_tail", "sequence" }`                                                                                                                                         |
| `adapter.close`       | `{ "adapter": "e1" }`                                                                                                                | `{ "closed": true }`                                                                                                                                                                                                                                |

`adapter.list` enumerates the built-in plugin manifests this server can instantiate. Each manifest may declare an optional `default_target = { program, args }`; `adapter.start` reads that field when the caller omits `program` (so e.g. `{"plugin": "claude-code"}` is enough to spawn the bundled claude-code adapter, since its manifest declares `"program": "claude"`). Plugins without a `default_target` require callers to pass `program` explicitly. `adapter.send` takes a plugin-defined `intent` string plus arbitrary JSON `params`; the server forwards them verbatim to the plugin and returns the post-apply classified state. `adapter.wait` defaults `intent` to `wait_turn_matcher` so simple callers can omit it, and defaults `timeout_ms` to `120000`. `adapter.inspect` applies the same body/status split the classifier uses (bottom three rows treated as status bar) so misclassification reports can be reproduced without standing up a parallel `session.*` connection.

`adapter.turn` is the atomic `send` + `wait` convenience — the same intent dispatch as `adapter.send` followed by the same matcher loop as `adapter.wait`, with the per-adapter mutex held across both legs so no other connection can slip an intent between them. The `wait` block may be omitted entirely (defaults to `wait_turn_matcher` with a 120 s timeout). Requires both `input.write` and `matcher.wait` permissions. **Head-of-line blocking warning:** the per-adapter mutex is held for the entire `wait` leg — every other RPC method that touches the same adapter from any connection blocks until `adapter.turn` returns. Use separate `adapter.send` + `adapter.wait` calls when finer scheduling matters.

`adapter.wait` and `adapter.turn` accept an optional client-minted `wait_id` (any string the caller chooses — UUIDv4 is conventional). When supplied, the server binds a `CancellationToken` to that id _before_ acquiring the per-adapter mutex, so a same-id `adapter.cancel_wait` from another connection can interrupt the wait even while the mutex is held. `adapter.cancel_wait { wait_id }` is idempotent — an unknown or already-completed id returns `cancelled: false` so callers don't have to race the wait to know whether the cancel landed. A successfully cancelled wait surfaces as the JSON-RPC `-32005 Cancelled` error on the originating call (distinct from `-32001 Timeout`).

#### Driving the built-in `claude-code` plugin

The Lua plugin exposes the following intents through `adapter.send`:

| Intent                                     | Params                                                            | Behaviour                                                                                                             |
| ------------------------------------------ | ----------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `send_prompt`                              | `{ "prompt": "..." }`                                             | Bracketed-pastes the prompt and presses Enter. Sets `last_intent = "prompt_submitted"`.                               |
| `approve`                                  | `{}`                                                              | Presses Enter to accept the current permission / plan-approval dialog.                                                |
| `deny`                                     | `{}`                                                              | Presses Escape to dismiss the current dialog.                                                                         |
| `choose_option`                            | `{ "index": 2 }`, `{ "option": "2" }`, or `{ "option": "label" }` | Types a numbered option and presses Enter. Label matching uses the options cached by the most recent classifier pass. |
| `cancel`                                   | `{}`                                                              | Sends Escape. Sets `last_intent = "cancelling"` so the classifier reports `cancelling` until the screen settles.      |
| `force_cancel`                             | `{}`                                                              | Sends Escape twice for tool calls already mid-flight. Same `last_intent = "cancelling"` as `cancel`.                  |
| `steer`                                    | `{ "prompt": "..." }`                                             | Mid-turn prompt injection without setting `last_intent = "prompt_submitted"`.                                         |
| `key`                                      | `{ "key": "..." }`                                                | Sends a named key alias or falls through to literal text for unrecognised short strings.                              |
| `approve_trust`                            | `{}`                                                              | Types `1` + Enter for the workspace-trust dialog (bare Enter does not accept option 1 in the Claude Code TUI).        |
| `deny_trust`                               | `{}`                                                              | Types `2` + Enter to deny workspace trust.                                                                            |
| `dismiss_welcome`                          | `{}`                                                              | Presses Enter to clear the first-launch welcome panel.                                                                |
| `expand`                                   | `{}`                                                              | Sends Ctrl+O to expand/collapse the focused tool-progress row.                                                        |
| `model_effort_left` / `model_effort_right` | `{}`                                                              | Sends Left / Right for model-picker effort controls.                                                                  |
| `slash_command`                            | `{ "command": "usage" }` or `{ "name": "usage" }`                 | Bracketed-pastes a slash command and presses Enter without marking a conversation turn.                               |
| `attach_file`                              | `{ "path": "..." }`                                               | Bracketed-pastes a file path into the prompt buffer without pressing Enter.                                           |

Plugin-defined state strings the classifier emits (matches the plugin's own `describe().states` catalog): `starting`, `ready`, `waiting_for_login`, `waiting_for_trust`, `waiting_for_model_select`, `waiting_for_enter_plan_mode`, `waiting_for_plan_approval`, `waiting_for_permission`, `waiting_for_external_editor`, `usage_screen`, `local_ui_screen`, `waiting_for_user_input`, `thinking`, `cancelling`, `completed_turn`, `error`. The host additionally synthesizes a `plugin_error` fallback state when a classifier call itself fails (e.g. Lua runtime error); this is not plugin-owned. The classifier also threads a separate `last_intent` string (e.g. `"prompt_submitted"`, `"cancelling"`) — that lives on the snapshot's `last_intent` field, not on `state`. The state vocabulary is owned by the Lua plugin — the Rust core does not interpret it.

See the [Extensions guide](../guide/extensions.md) for plugin authoring and the [Claude Code adapter guide](../guide/claude-code.md) for state semantics and limitations of the built-in claude-code plugin.

### Plugin methods

| Method                     | Params                                         | Result                                                           |
| -------------------------- | ---------------------------------------------- | ---------------------------------------------------------------- |
| `plugin.capabilities`      | none                                           | Host plugin capabilities.                                        |
| `plugin.validate_manifest` | `{ "manifest": { ... } }`                      | `{ "valid": true }`.                                             |
| `plugin.describe`          | `{ "plugin": "<name>" }`                       | `{ "plugin", "manifest", "intents", "wait_matchers", "states" }` |
| `plugin.load`              | `{ "manifest_path": "path/to/manifest.toml" }` | `{ "plugin": "<name>" }`.                                        |
| `plugin.unload`            | `{ "plugin": "<name>" }`                       | `{ "unloaded": true }`.                                          |

`plugin.capabilities` reports `embedded_lua: true` and includes built-in plugin manifests in `builtin_plugins`, including the `claude-code` Lua adapter. See [Plugins and extensions](./plugins.md) for manifest fields, runtime names, and permission names.

`plugin.describe` returns the catalog of intents, wait matchers, and classifier states a plugin advertises — used by consumers (the REPL completer, third-party drivers) to discover what a plugin supports without hard-coding intent names. Source of truth, in order:

1. If the plugin's Lua source exports a `describe()` function, the host returns its value verbatim. Plugins owning a richer catalog (per-intent param schemas, per-state descriptions) should provide one.
2. Otherwise the host introspects the exports table: function names ending with `_matcher` are listed under `wait_matchers`; `classify` and `describe` are filtered out; everything else lands under `intents`. `states` is `[]` in the fallback path because the classifier vocabulary lives inside the function body.

Each entry in `intents` / `wait_matchers` / `states` is at minimum `{ "name": "..." }`; plugins are free to add `description`, `params_schema`, or other fields a caller can opt into.

`plugin.load` registers a trusted-local third-party plugin from a TOML manifest file on disk. The manifest's `entrypoint` is resolved relative to the manifest file's parent directory; absolute paths, `..` components, and symlinks that resolve outside the manifest's own directory are all rejected. Built-in plugins (claude-code) cannot be replaced — a name collision returns an error.

`plugin.unload` deregisters a previously loaded third-party plugin. Built-in plugins cannot be unloaded. Plugins with live adapters bound to them are rejected; callers must `adapter.close` first.

Both methods require the server to have been started with `--allow-plugin-load`. Without that flag they return `-32004 PermissionDenied` with `data.reason = "server_did_not_grant_plugin_load"` so callers can distinguish this server-mode denial from per-adapter permission denials. The CLI `--plugin <manifest.toml>` flag on `ptywright serve` works independently — the operator is loading plugins at startup, which is explicitly trusted.

### Permission gating

Every `adapter.*` method consults the bound plugin manifest's declared `permissions` before invoking the handler. Methods that operate on an adapter require the matching `PluginPermission`:

| Method                                                   | Required permission               |
| -------------------------------------------------------- | --------------------------------- |
| `adapter.start`                                          | `session.spawn`                   |
| `adapter.resume`                                         | `session.spawn`                   |
| `adapter.send`                                           | `input.write`                     |
| `adapter.wait`                                           | `matcher.wait`                    |
| `adapter.turn`                                           | `input.write` AND `matcher.wait`  |
| `adapter.cancel_wait`                                    | none (caller-driven cancellation) |
| `adapter.snapshot` / `adapter.state` / `adapter.inspect` | `screen.read`                     |
| `adapter.transcript`                                     | `transcript.read`                 |
| `adapter.close`                                          | `session.kill`                    |
| `adapter.list` / `adapter.live`                          | none (read-only registry queries) |

When a manifest omits the required permission, the call returns `-32004 PermissionDenied` with structured `data` carrying `{ "method": "<name>", "required_permission": "<permission>" }`. Permissions are checked at dispatch time — an adapter's runtime privileges cannot be widened after `adapter.start`.

The built-in claude-code manifest declares all permission variants, so existing callers see no behaviour change. Third-party plugins loaded via `plugin.load` / `--plugin` are subject to their declared subset.

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

| Code     | Meaning                               |
| -------- | ------------------------------------- |
| `-32700` | Parse error.                          |
| `-32600` | Invalid request.                      |
| `-32601` | Method not found.                     |
| `-32602` | Invalid params.                       |
| `-32603` | Internal error.                       |
| `-32001` | Matcher timeout.                      |
| `-32002` | Session closed.                       |
| `-32004` | Permission denied (with `data` body). |
| `-32005` | Cancelled (cooperative cancellation). |
