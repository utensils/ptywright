# Claude Code adapter

ptywright includes an initial interactive Claude Code adapter built on the generic PTY/session/screen/action/matcher layers.

This adapter is intentionally scoped to the terminal TUI. It does not use or optimize for `claude -p`.

## Scope

Implemented now:

- Spawn `claude` interactively in a real PTY.
- Send prompts as terminal input.
- Classify coarse TUI states from screen/transcript evidence.
- Detect common permission, approval, thinking, and input prompt text.
- Approve, deny, or cancel with terminal key actions.
- Expose convenience JSON-RPC methods under the `claude.*` namespace.

Still evolving:

- Robust turn boundary detection across Claude Code UI changes.
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

Every state response includes:

- `state`
- `confidence`
- `evidence`
- `sequence`

The classifier is heuristic and deliberately isolated so Claude Code UI changes can be handled in one adapter module. Sanitized fixture tests cover ready, thinking, permission, plan approval, completed, and error-like screens.

## JSON-RPC methods

Claude methods are convenience wrappers. Generic `session.*` methods remain sufficient for clients that want full control.

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
- Approval and denial are terminal key actions; verify behavior against your installed Claude Code version.
- Screen/transcript evidence may contain sensitive project data. Avoid logging responses blindly.
