# Claude Code plugin

ptywright drives interactive TUIs through a generic [`adapter.*` JSON-RPC surface](../reference/json-rpc.md#adapter-methods) backed by the `Extension` trait. Claude Code is the first plugin shipped under that surface. There is no Rust shim for Claude Code — application-specific behaviour (state names, intent verbs, evidence strings) lives entirely in `plugins/claude-code/main.lua`.

This plugin is intentionally scoped to the terminal TUI. It does not use or optimize for `claude -p`. Rust provides the PTY controls and executes the generic action/matcher plans the Lua plugin returns.

## Scope

Implemented now:

- Spawn `claude` interactively in a real PTY from Rust or via `adapter.start`.
- Send prompts as terminal input through Lua-provided action plans.
- Classify coarse TUI states from screen/transcript evidence in Lua, using the body/status split the Rust host provides (bottom three rows are treated as status chrome unless a dialog structurally straddles the split).
- Detect common permission, enter-plan-mode, plan-approval, workspace-trust, model picker, usage, external-editor, local slash-command, thinking/tool-use, streaming, completed-turn, and input-prompt text.
- Approve, deny, cancel, select numbered options, adjust model effort, or send numeric trust-dialog selections through Lua-provided terminal key actions.
- Declare a default spawn target (`program = "claude"`) on the plugin manifest so callers can omit `program` on `adapter.start`.

Still evolving:

- Even stronger turn-boundary detection across Claude Code UI changes.
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

The classifier returns one of (matches the plugin's own `describe().states` catalog):

- `starting`
- `ready`
- `waiting_for_login`
- `waiting_for_trust`
- `waiting_for_model_select`
- `waiting_for_enter_plan_mode`
- `waiting_for_plan_approval`
- `waiting_for_permission`
- `waiting_for_external_editor`
- `usage_screen`
- `local_ui_screen`
- `waiting_for_user_input`
- `thinking`
- `cancelling`
- `completed_turn`
- `error`

The host additionally synthesizes a `plugin_error` fallback when a classifier call itself fails (Lua runtime error, malformed return value, etc.) — `plugin_error` is not plugin-owned.

`prompt_submitted` is not a state; it is the `last_intent` value the plugin's `send_prompt` action returns. The host threads it back into the next `ClassifyContext.last_intent` so the classifier can disambiguate (e.g. a stable `❯` prompt after `last_intent == "prompt_submitted"` resolves to `completed_turn`, but the same prompt without that hint stays `waiting_for_user_input`).

Every state response includes:

- `state`
- `confidence`
- `evidence`
- `sequence`
- `metadata` — plugin-specific structured details about the current state (see below)

### Metadata fields

The classifier may attach the following keys to `metadata`, depending on which state is detected. All fields are optional — callers should treat any missing key as "not observed this tick".

| Key          | When set                       | Shape                                                                                                                   |
| ------------ | ------------------------------ | ----------------------------------------------------------------------------------------------------------------------- |
| `status`     | always (status-bar row parsed) | `{ "model": "...", "permission_mode": "..." }`                                                                          |
| `permission` | `waiting_for_permission`       | `{ "tool": "Bash", "summary": "...", "options": ["Yes", "Yes, and don't ask again", "No"] }`                            |
| `enter_plan_mode` | `waiting_for_enter_plan_mode` | `{ "options": ["Yes, enter plan mode", "No, start implementing now"] }`                                              |
| `plan`       | `waiting_for_plan_approval`    | Multi-line plan body text (the bullet/step list Claude Code rendered). `metadata.options` may also list approval-mode choices such as auto mode, bypass permissions, or keep planning. |
| `error`      | `error`                        | `{ "kind": "rate_limit"\|"quota"\|"connection"\|"auth"\|"api"\|"unknown", "message": "...", "retry_after_s"?: number }` |
| `login`      | `waiting_for_login`            | `{ "url": "https://..." }` — extracted sign-in URL (bare-domain forms supported, host-suffix attacks rejected).         |
| `usage`      | `usage_screen` or completed `/usage` turn | Structured panel contents (cost/tokens/durations when visible, or limit/reset/extra-usage summaries for the settings-style usage tab). |
| `dialog_id`  | dialog-bearing states          | FNV-1a-keyed correlation id (see below).                                                                                |

`dialog_id` is stamped whenever the classifier sees a dialog (permission, plan approval, enter-plan-mode, trust, etc.) and surfaces at the top of the metadata object — `metadata.dialog_id` — alongside the matching dialog metadata block (`metadata.permission`, `metadata.plan`, …). Pass it back as `params.dialog_id` on the matching intent (`approve`, `deny`, `choose_option`, `approve_trust`, etc.) and the plugin will refuse to act on a stale dialog — returning a `stale_dialog` error rather than approving whatever Claude Code repainted between classify and act. Callers driving the plugin in a classify-then-act loop (the canonical `adapter.state` / `adapter.wait` / `adapter.send` cadence) get this for free; direct-Rust callers that fire intents without an intervening classify will see `_current_dialog_id == nil` and get `stale_dialog`, which is correct — that's the warning signal that the act-then-act path skipped the cross-check.

The classifier is heuristic and deliberately isolated in the Lua plugin so Claude Code UI changes can be handled without changing Rust PTY/session internals. Recorded fixture tests cover ready, thinking, tool-use/streaming, permission variants, enter-plan-mode, plan approval variants, model picker variants, the workspace-trust dialog, external-editor waits, suppressed-permission waits, local slash-command panels, interrupted, completed, usage, login, error-subtype, and error-like screens. Each fixture under `plugins/claude-code/fixtures/<name>.txt` carries a sibling `<name>.expected.json` describing the expected state, evidence string, optional `last_intent`, and confidence floor; adding a new fixture is a documentation-only change.

Current fixtures are based on sanitized captures from Claude Code v2.1.141 / v2.1.142 / v2.1.143 on Ghostty/macOS plus manually reconstructed screens from the older local Claude Code source snapshot under `~/Downloads/src`. Treat the exact labels, footer content, and slash-command layouts as versioned UI assumptions; update the Lua plugin and fixtures together when Claude Code changes its TUI.

`waiting_for_trust` is distinct from `waiting_for_permission`: the workspace-trust dialog presents a numbered list (`1 = Yes, proceed` / `2 = No, exit`) instead of the Bash/Edit-style "press Enter to approve" UI. A bare Enter does not accept option 1, so the Lua plugin exposes a separate `approve_trust` / `deny_trust` intent that types the numeric option first and then sends Enter.

`choose_option` is the generic selector intent for plugin-recognised numbered dialogs and lists. Pass either `{ "index": 2 }`, a numeric string such as `{ "option": "2" }`, or a label fragment such as `{ "option": "don't ask again" }` after a classify call has cached the visible options. It types the option number and presses Enter; the Rust host only sees generic text/key actions.

`adapter.wait` (default intent `wait_turn_matcher`) waits for both a turn-boundary indicator and a stable screen interval before classifying a submitted prompt as `completed_turn`. The primary turn-boundary anchor is the TUI's own end-of-turn marker — a `✻ <Verb> for <duration>` line (e.g. `✻ Brewed for 1s`, `✻ Worked for 5s`) that Claude Code renders only after the assistant has produced its final reply. Both completion branches (the stable-path used by `adapter.wait` and the poll-path used by `adapter.state`) require this marker so a preamble-before-tool-use pause — `⏺ I'll explore the project...` followed by an empty `❯` prompt for a single frame while the next spinner is between repaints — is not mistaken for turn completion. Other turn-boundary indicators include permission / approval / trust prompts, model picker panels, external-editor waits, local UI screens, and stable slash-command output such as `/usage`. A plain prompt glyph without stable-screen evidence is classified as `waiting_for_user_input`.

## JSON-RPC

Drive the plugin entirely through [`adapter.*`](../reference/json-rpc.md#adapter-methods). The plugin manifest declares `default_target.program = "claude"`, so `adapter.start` only needs `{"plugin": "claude-code"}`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "adapter.start",
  "params": { "plugin": "claude-code", "cwd": "/repo" }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "adapter.send",
  "params": {
    "adapter": "e1",
    "intent": "send_prompt",
    "params": { "prompt": "implement the next test" }
  }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "adapter.wait",
  "params": { "adapter": "e1", "timeout_ms": 120000 }
}
```

Plugin intents available via `adapter.send`:

| Intent            | Params                                                          | Notes                                                                                                                                                                                                        |
| ----------------- | --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `send_prompt`     | `{ "prompt": "..." }`                                           | Bracketed-pastes the prompt and presses Enter. Sets `last_intent = "prompt_submitted"`.                                                                                                                      |
| `approve`         | `{}`                                                            | Presses Enter to accept the current permission / plan-approval dialog.                                                                                                                                       |
| `deny`            | `{}`                                                            | Presses Escape to dismiss the current dialog.                                                                                                                                                                |
| `choose_option`   | `{ "index": 2 }`, `{ "option": "2" }`, or `{ "option": "label" }` | Types a numbered option and presses Enter. Label matching uses the options cached by the most recent classifier pass, so use it after `adapter.state` / `adapter.wait`. Accepts optional `dialog_id`.        |
| `cancel`          | `{}`                                                            | Sends Escape (Claude Code 2.1.x's documented mid-turn interrupt key). Sets `last_intent = "cancelling"`.                                                                                                     |
| `force_cancel`    | `{}`                                                            | Sends Escape twice for tool calls already mid-API-request when the first Escape arrives. Same `last_intent = "cancelling"` as `cancel`.                                                                      |
| `steer`           | `{ "prompt": "..." }`                                           | Mid-turn prompt injection — same bytes as `send_prompt` but does NOT flip `last_intent` to `prompt_submitted`, so the classifier keeps waiting for the original turn to actually finish.                     |
| `key`             | `{ "key": "..." }`                                              | Generic single-key/text send: aliases (`enter`, `escape`, `tab`, `ctrl_c`, `shift_tab`, `f1`–`f12`, etc., with `-` or `_` separators) route to `action.key`; anything else falls through to `action.text`.   |
| `approve_trust`   | `{}`                                                            | Types `1` + Enter for the workspace-trust dialog.                                                                                                                                                            |
| `deny_trust`      | `{}`                                                            | Types `2` + Enter for the workspace-trust dialog.                                                                                                                                                            |
| `dismiss_welcome` | `{}`                                                            | Presses Enter to clear the first-launch welcome panel.                                                                                                                                                       |
| `expand`          | `{}`                                                            | Sends Ctrl+O — toggles expansion of the focused collapsible row (`Reading N files…`, search results, Bash output). Mirrors Claude Code 2.1.x's keyboard binding so callers don't have to remember the alias. |
| `model_effort_left` / `model_effort_right` | `{}`                                          | Sends Left / Right for model-picker effort controls.                                                                                                                                                         |
| `slash_command`   | `{ "command": "btw" }` (or `{"name":...}`, accepts leading `/`) | Bracketed-pastes `/<name>` and presses Enter. Does NOT flip `last_intent` to `prompt_submitted` because slash commands open a panel/modal, they aren't conversation turns. Empty `command` is a no-op.       |
| `attach_file`     | `{ "path": "..." }`                                             | Bracketed-pastes a file path into the prompt buffer so Claude Code's drag-and-drop attach path can pick it up. Rejects empty paths and paths with embedded newlines. Does NOT submit (no Enter).             |

Diagnostic reads — `adapter.snapshot`, `adapter.transcript`, `adapter.inspect` — work the same way as their `session.*` counterparts and redact by default. `adapter.inspect` additionally returns the `body_text` / `status_text` split the classifier sees, so misclassification reports can be reproduced without spinning up a parallel `session.*` connection.

## Streaming demo

`scripts/claude-stream.py` is a single-command driver that spawns `ptywright serve --stdio`, starts the built-in `claude-code` adapter, auto-handles the workspace-trust dialog, submits a prompt, and streams the live screen body + state transitions until the turn completes. It is wired into the Nix devshell as the `claude-stream` command:

```bash
nix develop --command claude-stream "List exactly three .rs files under src/ and report their paths."
# or read the prompt from a file / stdin:
claude-stream @./prompt.md
echo "what changed since last week?" | claude-stream -
```

Defaults that matter:

- `--model sonnet` — Sonnet engages with tool-using prompts reliably; switch to `--model haiku` for fast smoke tests, accepting that Haiku sometimes acknowledges-and-stops on broad prompts.
- `--timeout 600` — the outer hard deadline. Almost every internal wait is driven by screen-content evidence (`adapter.wait` matchers, `session.exited` notifications, classifier state transitions). The one exception is the bounded initial settle wait in `submit()` — capped at 3 s before the prompt-acknowledgement loop takes over — which is skipped entirely when the startup loop already observed `waiting_for_user_input`.
- `--heartbeat-ms 50` — poll cadence. Pulses `session.output` redraws but the script does not itself depend on this being any particular value.

What the script demonstrates (and what an integrator should copy):

1. Bounded settle wait before submission, so `send_prompt` doesn't race a paste against a not-yet-rendered prompt.
2. Auto-approval of the workspace-trust dialog via the plugin's `approve_trust` intent.
3. Single deterministic `completed_turn` exit signal — no consecutive-poll debouncing, no minimum-bytes thresholds, just the classifier's view of the TUI's own end-of-turn marker.
4. Fallback dump of the body's answer region if the streaming diff caught nothing (covers the edge case where the model produces its entire reply between two polling ticks).
5. SIGINT handler that terminates the ptywright subprocess and lets the main thread clean up RPC state outside the signal context.

The script doubles as a worked example of every primitive a real consumer would touch: `adapter.start`, `adapter.send`, `adapter.state`, `adapter.inspect`, `adapter.transcript`, `adapter.wait`, `adapter.close`, plus the `session.output` / `session.exited` notification subscription. Most of the file is comments documenting why each guard exists; the actual control flow is short. The test surface lives in `tests/scripts/test_claude_stream.py` (stdlib `unittest`) and locks down the chrome filter, the answer-region fallback, the terminal-state taxonomy, and the JSON-RPC framing contracts that determine whether the streaming output is legible and how failures terminate.

## Required environment

The plugin manifest declares two `default_target.required_env` keys that callers cannot override on `adapter.start`:

- `CLAUDE_CODE_DISABLE_TERMINAL_TITLE = "1"` — without this, OSC title-set escapes appear in the screen body and pollute the body-text classifier.
- `CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL = "1"` — without this, virtual scrolling moves cells out of the vt100 grid the classifier inspects.

See the [environment merge precedence](../reference/plugins.md#environment-merge-precedence) in the plugins reference for the manifest-vs-caller-vs-required-env ladder.

## Safety and limitations

- The plugin drives whatever `claude` executable is found on `PATH` unless `program` is overridden on `adapter.start`.
- Approval, denial, and trust selections are terminal key actions selected by the Lua plugin; verify behavior against your installed Claude Code version.
- Lua runs only on explicit adapter calls (classify / plan / wait_matcher), not per PTY byte.
- Screen/transcript evidence may contain sensitive project data. Reads through `adapter.snapshot`, `adapter.transcript`, and `adapter.inspect` redact by default; pass `"redact": false` for raw output in trusted local debugging.
