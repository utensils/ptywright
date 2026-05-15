# Claude Code adapter

ptywright drives interactive TUIs through a generic [`adapter.*` JSON-RPC surface](../reference/json-rpc.md#generic-adapter-methods) backed by the `Extension` trait. Claude Code is the first plugin shipped under that surface; the original `claude.*` aliases continue to work for callers that already speak them.

This adapter is intentionally scoped to the terminal TUI. It does not use or optimize for `claude -p`. Claude Code-specific decisions live in the built-in Lua plugin at `plugins/claude-code/main.lua`; Rust provides the PTY controls and executes the generic action/matcher plans the plugin returns.

## Scope

Implemented now:

- Spawn `claude` interactively in a real PTY from Rust or via `adapter.start` / `claude.start`.
- Send prompts as terminal input through Lua-provided action plans.
- Classify coarse TUI states from screen/transcript evidence in Lua, against the body region of the screen only (the bottom three status-bar rows are excluded from body classification).
- Detect common permission, plan-approval, workspace-trust, thinking/tool-use, streaming, completed-turn, and input-prompt text.
- Approve, deny, cancel, or send numeric trust-dialog selections through Lua-provided terminal key actions.
- Expose convenience JSON-RPC methods under the `claude.*` namespace alongside the generic `adapter.*` surface.

Still evolving:

- Even stronger turn-boundary detection across Claude Code UI changes.
- More detailed permission and plan prompt parsing.
- Event subscriptions for state transitions.
- Broader recorded screen fixtures from real Claude Code sessions.

## Rust API

```rust
use std::time::Duration;
use ptywright::{ClaudeCodeAdapter, ClaudeCodeConfig};

let mut claude = ClaudeCodeAdapter::start(ClaudeCodeConfig::default())?;
let state = claude.send_prompt("summarize this repository")?;
let next = claude.wait_turn(Duration::from_secs(120))?;
# Ok::<(), ptywright::Error>(())
```

`ClaudeCodeConfig::default()` launches `claude` with no `-p`/`--print` argument and a 40x120 terminal. Under the hood `ClaudeCodeAdapter` is a typed wrapper around `ExtensionHandle` with the built-in `claude-code` `LuaExtension`; the wrapper translates the plugin's state strings into the typed `ClaudeCodeState` enum.

## State model

The classifier returns:

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

`claude.wait_turn` waits for both a turn-boundary indicator and a stable screen interval before classifying a submitted prompt as `completed_turn`. Turn-boundary indicators include prompt lines, permission/approval/trust prompts, and stable slash-command output such as `/usage`. A plain prompt glyph without stable-screen evidence is classified as `waiting_for_user_input`.

## JSON-RPC methods

The generic [`adapter.*` surface](../reference/json-rpc.md#generic-adapter-methods) is the recommended entry point for new clients: pass `plugin: "claude-code"` to `adapter.start`, then drive the handle with `adapter.send` / `adapter.wait` / `adapter.state`. The `claude.*` methods listed below are the original Claude-specific aliases. They still work and share the same underlying `ExtensionHandle` semantics. At the moment, `claude.*` and `adapter.*` route through separate registries inside the server; that internal consolidation is tracked as a follow-up.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "claude.start",
  "params": { "cwd": "/repo", "rows": 40, "cols": 120 }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "claude.send_prompt",
  "params": { "claude": "c1", "prompt": "implement the next test" }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "claude.wait_turn",
  "params": { "claude": "c1", "timeout_ms": 120000 }
}
```

Available `claude.*` methods:

- `claude.start`
- `claude.send_prompt`
- `claude.wait_turn`
- `claude.approve`
- `claude.deny`
- `claude.cancel`
- `claude.state`
- `claude.snapshot` — passthrough to the adapter's underlying session snapshot
- `claude.transcript` — passthrough to the adapter's underlying session transcript
- `claude.inspect` — diagnostic dump that returns the current state plus the body/status split the classifier would see

`claude.approve` and `claude.deny` return both the post-apply state snapshot and a deprecated boolean alias:

```json
{
  "state": {
    "state": "completed_turn",
    "confidence": 0.9,
    "evidence": "...",
    "sequence": 17
  },
  "approved": true
}
```

Read `state` like every other mutation method. The `approved` / `denied` booleans are retained so existing callers that pattern-match the old `{approved: true}` / `{denied: true}` shape keep working; treat them as deprecated.

## Safety and limitations

- The adapter drives whatever `claude` executable is found on `PATH` unless `program` is overridden.
- Approval, denial, and trust selections are terminal key actions selected by the Lua adapter; verify behavior against your installed Claude Code version.
- Lua runs only on explicit adapter calls, not per PTY byte.
- Screen/transcript evidence may contain sensitive project data. Reads through `claude.snapshot`, `claude.transcript`, and `claude.inspect` redact by default; pass `"redact": false` for raw output in trusted local debugging.
