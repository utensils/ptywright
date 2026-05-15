# Claude Code plugin

ptywright drives interactive TUIs through a generic [`adapter.*` JSON-RPC surface](../reference/json-rpc.md#adapter-methods) backed by the `Extension` trait. Claude Code is the first plugin shipped under that surface. There is no Rust shim for Claude Code — application-specific behaviour (state names, intent verbs, evidence strings) lives entirely in `plugins/claude-code/main.lua`.

This plugin is intentionally scoped to the terminal TUI. It does not use or optimize for `claude -p`. Rust provides the PTY controls and executes the generic action/matcher plans the Lua plugin returns.

## Scope

Implemented now:

- Spawn `claude` interactively in a real PTY from Rust or via `adapter.start`.
- Send prompts as terminal input through Lua-provided action plans.
- Classify coarse TUI states from screen/transcript evidence in Lua, against the body region of the screen only (the bottom three status-bar rows are excluded from body classification).
- Detect common permission, plan-approval, workspace-trust, thinking/tool-use, streaming, completed-turn, and input-prompt text.
- Approve, deny, cancel, or send numeric trust-dialog selections through Lua-provided terminal key actions.
- Declare a default spawn target (`program = "claude"`) on the plugin manifest so callers can omit `program` on `adapter.start`.

Still evolving:

- Even stronger turn-boundary detection across Claude Code UI changes.
- More detailed permission and plan prompt parsing.
- Event subscriptions for state transitions.
- Broader recorded screen fixtures from real Claude Code sessions.

## Rust API

There is no `ClaudeCodeAdapter`. Drive the plugin through `ExtensionHandle` over the generic `Extension` trait:

```rust
use std::time::Duration;
use serde_json::json;

use ptywright::{
    ExtensionHandle, LuaExtension, Session, SessionConfig, Target, TerminalSize,
};

let target = Target::new("claude").size(TerminalSize::new(40, 120));
let session = Session::spawn(SessionConfig::new(target))?;
let extension = LuaExtension::built_in("claude-code")?;
let mut handle = ExtensionHandle::start(Box::new(extension), session, 300);

let state = handle.send("send_prompt", json!({ "prompt": "summarize this repository" }))?;
let next = handle.wait("wait_turn_matcher", json!({}), Duration::from_secs(120))?;
println!("state={}, evidence={}", next.state, next.evidence);
# Ok::<(), ptywright::Error>(())
```

`state.state` is a plugin-defined string (see the State model below); the Rust core does not interpret it. Rust callers that want a typed enum should define one locally and convert from the plugin's state string — that translation is application-specific and intentionally not part of the library surface.

## State model

The classifier returns one of:

- `starting`
- `ready`
- `prompt_submitted`
- `thinking`
- `waiting_for_permission`
- `waiting_for_plan_approval`
- `waiting_for_trust`
- `waiting_for_user_input`
- `completed_turn`
- `cancelling`
- `exited`
- `error`
- `plugin_error`

Every state response includes:

- `state`
- `confidence`
- `evidence`
- `sequence`

The classifier is heuristic and deliberately isolated in the Lua plugin so Claude Code UI changes can be handled without changing Rust PTY/session internals. Recorded fixture tests cover ready, thinking, tool-use/streaming, permission variants, plan approval variants, the workspace-trust dialog, interrupted, completed, usage, and error-like screens. Each fixture under `tests/fixtures/claude_code/<name>.txt` carries a sibling `<name>.expected.json` describing the expected state, evidence string, optional `last_intent`, and confidence floor; adding a new fixture is a documentation-only change.

Current fixtures are based on sanitized captures from Claude Code v2.1.141 / v2.1.142 on Ghostty/macOS. Treat the exact labels, footer content, and slash-command layouts as versioned UI assumptions; update the Lua plugin and fixtures together when Claude Code changes its TUI.

`waiting_for_trust` is distinct from `waiting_for_permission`: the workspace-trust dialog presents a numbered list (`1 = Yes, proceed` / `2 = No, exit`) instead of the Bash/Edit-style "press Enter to approve" UI. A bare Enter does not accept option 1, so the Lua plugin exposes a separate `approve_trust` / `deny_trust` intent that types the numeric option first and then sends Enter.

`adapter.wait` (default intent `wait_turn_matcher`) waits for both a turn-boundary indicator and a stable screen interval before classifying a submitted prompt as `completed_turn`. Turn-boundary indicators include prompt lines, permission/approval/trust prompts, and stable slash-command output such as `/usage`. A plain prompt glyph without stable-screen evidence is classified as `waiting_for_user_input`.

## JSON-RPC

Drive the plugin entirely through [`adapter.*`](../reference/json-rpc.md#adapter-methods). The plugin manifest declares `default_target.program = "claude"`, so `adapter.start` only needs `{"plugin": "claude-code"}`:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "adapter.start", "params": { "plugin": "claude-code", "cwd": "/repo" } }
```

```json
{ "jsonrpc": "2.0", "id": 2, "method": "adapter.send",
  "params": { "adapter": "e1", "intent": "send_prompt", "params": { "prompt": "implement the next test" } } }
```

```json
{ "jsonrpc": "2.0", "id": 3, "method": "adapter.wait",
  "params": { "adapter": "e1", "timeout_ms": 120000 } }
```

Plugin intents available via `adapter.send`:

| Intent             | Params                | Notes                                                                                                                                       |
| ------------------ | --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `send_prompt`      | `{ "prompt": "..." }` | Bracketed-pastes the prompt and presses Enter. Sets `last_intent = "prompt_submitted"`.                                                      |
| `approve`          | `{}`                  | Presses Enter to accept the current permission / plan-approval dialog.                                                                      |
| `deny`             | `{}`                  | Presses Escape to dismiss the current dialog.                                                                                               |
| `cancel`           | `{}`                  | Sends Ctrl-C. Sets `last_intent = "cancelling"`.                                                                                            |
| `approve_trust`    | `{}`                  | Types `1` + Enter for the workspace-trust dialog.                                                                                           |
| `deny_trust`       | `{}`                  | Types `2` + Enter for the workspace-trust dialog.                                                                                           |
| `dismiss_welcome`  | `{}`                  | Presses Enter to clear the first-launch welcome panel.                                                                                      |

Diagnostic reads — `adapter.snapshot`, `adapter.transcript`, `adapter.inspect` — work the same way as their `session.*` counterparts and redact by default. `adapter.inspect` additionally returns the `body_text` / `status_text` split the classifier sees, so misclassification reports can be reproduced without spinning up a parallel `session.*` connection.

## Safety and limitations

- The plugin drives whatever `claude` executable is found on `PATH` unless `program` is overridden on `adapter.start`.
- Approval, denial, and trust selections are terminal key actions selected by the Lua plugin; verify behavior against your installed Claude Code version.
- Lua runs only on explicit adapter calls (classify / plan / wait_matcher), not per PTY byte.
- Screen/transcript evidence may contain sensitive project data. Reads through `adapter.snapshot`, `adapter.transcript`, and `adapter.inspect` redact by default; pass `"redact": false` for raw output in trusted local debugging.
