# Claude Code adapter

ptywright includes an interactive Claude Code adapter built on the generic PTY/session/screen/action/matcher layers.

This adapter is intentionally scoped to the terminal TUI. It does not use or optimize for `claude -p`. Claude Code-specific decisions now live in the built-in Lua plugin at `plugins/claude-code/main.lua`; Rust provides the PTY controls and executes the generic action/matcher plans returned by Lua.

## Scope

Implemented now:

- Spawn `claude` interactively in a real PTY from Rust.
- Send prompts as terminal input through Lua-provided action plans.
- Classify coarse TUI states from screen/transcript evidence in Lua.
- Detect common permission, approval, thinking/tool-use, streaming, and input prompt text in Lua.
- Approve, deny, or cancel with Lua-provided terminal key actions.
- Expose convenience JSON-RPC methods under the `claude.*` namespace.

Still evolving:

- Even stronger turn boundary detection across Claude Code UI changes.
- More detailed permission and plan prompt parsing.
- Event subscriptions for state transitions.
- Broader golden screen fixtures from real Claude Code sessions.

## Rust API

```rust
use std::time::Duration;
use ptywright::{ClaudeCodeAdapter, ClaudeCodeConfig};

let mut claude = ClaudeCodeAdapter::start(ClaudeCodeConfig::default())?;
let state = claude.send_prompt("summarize this repository")?;
let next = claude.wait_turn(Duration::from_secs(120))?;
# Ok::<(), ptywright::Error>(())
```

`ClaudeCodeConfig::default()` launches `claude` with no `-p`/`--print` argument and a 40x120 terminal.

## State model

The current state classifier returns:

- `starting`
- `ready`
- `prompt_submitted`
- `thinking`
- `waiting_for_permission`
- `waiting_for_plan_approval`
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

The classifier is heuristic and deliberately isolated in the Lua plugin so Claude Code UI changes can be handled without changing Rust PTY/session internals. Sanitized fixture tests cover ready, thinking, tool-use/streaming, permission variants, plan approval variants, interrupted, completed, usage, and error-like screens.

Current fixtures are based on sanitized captures from Claude Code v2.1.141 on Ghostty/macOS with Sonnet 4.6 and Opus 4.7 displays. Treat the exact labels, footer content, and slash-command layouts as versioned UI assumptions; update the Lua plugin and fixtures together when Claude Code changes its TUI.

`claude.wait_turn` waits for both a turn-boundary indicator and a stable screen interval before classifying a submitted prompt as `completed_turn`. Turn-boundary indicators include prompt lines, permission/approval prompts, and stable slash-command output such as `/usage`. A plain prompt glyph without stable-screen evidence is classified as `waiting_for_user_input`.

## JSON-RPC methods

Claude methods are compatibility/convenience wrappers around the built-in Lua adapter. Generic `session.*` methods remain sufficient for clients that want full control.

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

Other methods:

- `claude.approve`
- `claude.deny`
- `claude.cancel`
- `claude.state`

## Safety and limitations

- The adapter drives whatever `claude` executable is found on `PATH` unless `program` is overridden.
- Approval and denial are terminal key actions selected by the Lua adapter; verify behavior against your installed Claude Code version.
- Lua runs only on explicit adapter calls, not per PTY byte.
- Screen/transcript evidence may contain sensitive project data. Avoid logging responses blindly.
