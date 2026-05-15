---
name: ptywright
description: Drive interactive terminal applications through PTYs using the ptywright CLI. Use when asked to spawn or attach to a TUI, capture terminal screen state, send key sequences or text into a PTY, wait for prompts or screen-stable evidence, automate Claude Code interactively (start, send a prompt, approve/deny, wait for turn boundaries), or run ptywright's JSON-RPC server over stdio or a local socket.
argument-hint: [subcommand or question]
allowed-tools: Bash, Read, Glob, Grep
---

# ptywright — Headless PTY/TUI Automation CLI

ptywright spawns interactive terminal programs in a real PTY, parses their screen state with a `vt100`-backed engine, sends input as deterministic actions, and waits on screen/transcript/lifecycle matchers. It exposes a JSON-RPC 2.0 server over stdio or a local socket so agents can drive any TUI — and ships a built-in Lua plugin (`claude-code`) for interactive Claude Code automation. There is no per-application Rust shim; every TUI plugin is driven through the generic `adapter.*` JSON-RPC surface.

## Quick Reference

```bash
ptywright --help                                              # Top-level help
ptywright --version                                           # Version
ptywright run -- /bin/sh -lc 'printf ready'                   # Live PTY bridge (debug only)
ptywright run --rows 40 --cols 120 -- claude                  # Drive a TUI interactively
ptywright serve --stdio                                       # JSON-RPC over stdio, NDJSON framing
ptywright serve --stdio --framing lsp                         # JSON-RPC over stdio, LSP framing
ptywright serve --socket /tmp/ptywright.sock                  # Local IPC, multi-client
ptywright serve --socket '\\.\pipe\ptywright'                 # Windows named pipe (PowerShell)
ptywright completions zsh                                     # Generate completions for a shell
```

## How to Use This Skill

Parse `$ARGUMENTS` to pick a path:

- If the user describes a **TUI to drive** (e.g. "spawn `htop` and wait until the header renders", "drive Claude Code through a prompt"), use the `serve --stdio` JSON-RPC server piped through `jq`. Never use `ptywright run` for automation — `run` is a live tty bridge for humans.
- If the user mentions **Claude Code**, use the generic `adapter.*` surface with `plugin: "claude-code"` rather than rebuilding turn detection from `session.*` primitives. See the [Claude Code plugin pattern](#drive-claude-code-end-to-end) and [test matrix](#claude-code-plugin-test-matrix) — this is the most-exercised path in the project today.
- If `$ARGUMENTS` starts with a **subcommand** (`run`, `serve`, `completions`), pass it through.
- If `$ARGUMENTS` is empty, run `ptywright --help`.

For agents: the JSON-RPC server is the automation surface. Wrap calls in `printf '%s\n' '<request>' | ptywright serve --stdio` for one-shots, or speak to a long-lived `serve --socket` instance for multi-step turns.

## CLI Surface

### `ptywright run -- <cmd>`

Live stdin/stdout PTY bridge. Forwards your keyboard to the child and streams the child's bytes back to your terminal. Use for **manual smoke testing** only: stdout is raw terminal bytes and the program owns the foreground, so anything you write to stdout from the wrapping shell can race the child.

```bash
ptywright run -- /bin/sh -lc 'echo hello'
ptywright run --rows 40 --cols 120 -- claude
```

Exit status mirrors the child's. Logs go to `~/.ptywright/logs/ptywright.YYYY-MM-DD.log` only — never stderr or stdout in this mode, because either would corrupt the live terminal.

### `ptywright serve --stdio`

JSON-RPC 2.0 over stdin/stdout. Stdout is **protocol only** (no logs, no banners), stderr carries human diagnostics. Two framings:

| Framing | Flag | Description |
| --- | --- | --- |
| NDJSON | `--framing ndjson` (default) | One JSON-RPC message per line. Easy to pipe through `jq`. |
| LSP | `--framing lsp` | `Content-Length: N\r\n\r\n<payload>`. Binary-safe; matches the Language Server Protocol envelope. |

Single connection, single-process. The server exits when stdin closes.

### `ptywright serve --socket <path>`

Same protocol, but over a local IPC endpoint:

- **macOS/Linux**: Unix domain socket. Stale socket files are removed on startup; non-socket files are refused.
- **Windows**: named pipe via the `interprocess` crate. Use a name like `\\.\pipe\ptywright`.

Multiple clients can connect concurrently. **Only `session.*` ids are shared across connections** (via `RpcServerState`); `adapter.*` extension ids live in per-connection registries and cannot be looked up from a sibling socket client. If you need a long-lived adapter handle, keep one client open and serialise calls through it. Each connection also owns its own framing/subscription state.

### `ptywright completions <shell>`

Generates static completions for `bash`, `zsh`, `fish`, `elvish`, `powershell`. The zsh output enables clap's dynamic completer for richer suggestions:

```bash
source <(ptywright completions zsh)                    # zsh / bash
ptywright completions fish | source                    # fish
ptywright completions fish > ~/.config/fish/completions/ptywright.fish
ptywright completions powershell | Out-String | Invoke-Expression  # PowerShell
```

## JSON-RPC Server

### Capabilities probe

Every script should start by reading `server.capabilities` to confirm the binary speaks the methods you expect:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}' \
  | ptywright serve --stdio \
  | jq '.result.methods'
```

Live shape (verified against ptywright 0.1.0):

```json
{
  "name": "ptywright",
  "version": "0.1.0",
  "framing": ["ndjson", "lsp"],
  "methods": ["server.capabilities", "server.set_notifications",
              "session.create", "session.list", "session.close",
              "session.kill", "session.resize", "session.input",
              "session.snapshot", "session.transcript", "session.wait",
              "adapter.list", "adapter.start", "adapter.state",
              "adapter.send", "adapter.wait", "adapter.snapshot",
              "adapter.transcript", "adapter.inspect", "adapter.close",
              "plugin.capabilities", "plugin.validate_manifest"],
  "notifications": ["session.changed", "session.exited"]
}
```

### Method catalog

#### `server.*`

| Method | Params | Returns | Notes |
| --- | --- | --- | --- |
| `server.capabilities` | — | name/version/methods/notifications/framing | Always cheap; safe to call any time. |
| `server.set_notifications` | `{enabled: bool}` | `{enabled}` | Opt in to server-originated `session.changed` / `session.exited` notifications on this connection. Responses are written before queued notifications. |

#### `session.*` — generic PTY automation

| Method | Params | Returns |
| --- | --- | --- |
| `session.create` | `{program, args?, cwd?, env?, rows?, cols?, pixel_width?, pixel_height?, transcript_max_chars?, raw_transcript_path?, raw_transcript_append?}` | `{session}` |
| `session.list` | — | `{sessions: [id, ...]}` |
| `session.close` | `{session}` | `{closed: true}` |
| `session.kill` | `{session}` | `{killed: true}` |
| `session.resize` | `{session, rows, cols, pixel_width?, pixel_height?}` | `{resized: true}` |
| `session.input` | `{session, action}` | `{sent: true}` |
| `session.snapshot` | `{session, redact?, redaction?}` | `ScreenSnapshot` |
| `session.transcript` | `{session, redact?, redaction?}` | `{text}` |
| `session.wait` | `{session, matcher, timeout_ms?}` | `{matched, sequence, elapsed_ms, snapshot, transcript_tail}` |

Defaults: `rows=24, cols=80, timeout_ms=30000`. Transcript is bounded in memory (128 KiB UTF-8 by default; tune via `transcript_max_chars`). `raw_transcript_path` enables raw file streaming and requires `raw_transcript_append` when re-opening an existing path.

#### `adapter.*` — generic plugin-backed surface

The single plugin-driving surface. Any built-in plugin (today, `claude-code`) is driven through it. The same `intent` plus `params` shape works across plugins; no per-application RPC namespace exists.

| Method | Params | Returns |
| --- | --- | --- |
| `adapter.list` | — | `{plugins: [PluginManifest, ...]}` |
| `adapter.start` | `{plugin, program?, args?, cwd?, env?, rows?, cols?, pixel_width?, pixel_height?}` | `{adapter, plugin, state}` |
| `adapter.state` | `{adapter}` | `{state}` |
| `adapter.send` | `{adapter, intent, params?}` | `{state}` |
| `adapter.wait` | `{adapter, intent?="wait_turn_matcher", params?, timeout_ms?=120000}` | `{state}` |
| `adapter.snapshot` | `{adapter, redact?, redaction?}` | `ScreenSnapshot` |
| `adapter.transcript` | `{adapter, redact?, redaction?}` | `{text}` |
| `adapter.inspect` | `{adapter, redact?, redaction?}` | `{adapter, plugin, state, plain_text, body_text, status_text, transcript_tail, sequence}` |
| `adapter.close` | `{adapter}` | `{closed: true}` |

`adapter.start` requires `plugin` (e.g. `"claude-code"`). Each plugin manifest may declare an optional `default_target = {program, args}`; `adapter.start` reads that field when the caller omits `program`. The bundled `claude-code` plugin declares `default_target.program = "claude"`, so `{"plugin": "claude-code"}` is enough to spawn it. Plugins without a `default_target` require the caller to pass `program` explicitly. `adapter.send` takes a plugin-defined `intent` string plus arbitrary JSON `params` — for the claude-code plugin the intents are `send_prompt`, `approve`, `deny`, `cancel`, `approve_trust`, `deny_trust`, `dismiss_welcome`. `adapter.inspect` applies the same body/status split the classifier uses, so you can reproduce a misclassification report without standing up a parallel `session.*` connection.

`adapter.wait` automatically injects the host's configured `completed_turn_stable_ms` (300 ms by default) into the matcher params if the caller does not supply one, so generic callers can omit `params` entirely and still get stable-screen turn boundaries — the matcher only fires after the screen has been quiet for the configured window.

#### Driving the built-in `claude-code` plugin

The plugin classifies states (`starting`, `ready`, `prompt_submitted`, `thinking`, `waiting_for_permission`, `waiting_for_plan_approval`, `waiting_for_trust`, `waiting_for_user_input`, `completed_turn`, `cancelling`, `exited`, `error`, `plugin_error`) using **stable-screen evidence** rather than raw string matches — that's the whole point of going through Lua plus the screen engine. State strings are plugin-defined; the Rust core does not interpret them.

`waiting_for_trust` is the workspace-trust dialog: send `intent: "approve_trust"` / `"deny_trust"` through `adapter.send` (they type `1`+Enter or `2`+Enter, since a bare Enter does not accept option 1 on the numbered list).

After a fresh-launch `approve_trust`, Claude Code shows a welcome panel with "Welcome back …", "Tips for getting started", and "What's new". The classifier reports this as `starting` (evidence: `welcome screen visible`) rather than `waiting_for_user_input`, because Claude treats the first keypress on that panel as a dismissal rather than a prompt submission. Send `intent: "dismiss_welcome"` through `adapter.send` (or call `send_prompt` directly — bracketed-paste'd content dismisses the welcome and submits in one step) before treating the adapter as ready for prompts.

`send_prompt` writes the prompt as a `bracketed_paste` action (`CSI 200 ~` … `CSI 201 ~`) so Claude Code v2.1+ treats the payload as a single paste and a trailing Enter as a real submit. The generic `paste` action still writes raw bytes — only use `bracketed_paste` against programs that have enabled bracketed paste (Claude Code v2.1+, vim, fish, …); against `cat` or a plain shell the wrapper bytes would land in the child as literal characters.

> **Body/status split.** The classifier runs against a body/status split (`STATUS_BAR_ROWS = 3` rows treated as status) so benign status strings like `⏵⏵ bypass permissions on (shift+tab to cycle)` cannot false-positive on substring matches in the body. The `idle_bypass_permissions.txt` fixture under `tests/fixtures/claude_code/` locks this in. Each fixture has a sibling `.expected.json` describing the expected state, evidence, optional `last_intent`, and confidence floor; the regression test in `tests/lua_classifier_tests.rs` auto-enrols every fixture, so adding a new capture is a single-file change. Use `adapter.inspect` to dump the body/status view the classifier sees when investigating new misclassifications.

#### `plugin.*`

| Method | Params | Returns |
| --- | --- | --- |
| `plugin.capabilities` | — | host capabilities including `embedded_lua` and `builtin_plugins` |
| `plugin.validate_manifest` | `{manifest}` | structural validation result |

### Action shape

`session.input` accepts a tagged `Action`. JSON tag = `type`, payload = `value`:

```json
{"type":"text","value":"hello"}
{"type":"key","value":"enter"}                         // enter|escape|tab|backspace|up|down|left|right|ctrl_c|ctrl_d
{"type":"paste","value":"multiline\npaste"}
{"type":"bracketed_paste","value":"goes wrapped in CSI 200~ ... CSI 201~"}
{"type":"resize","value":{"rows":40,"cols":120,"pixel_width":0,"pixel_height":0}}
{"type":"interrupt"}                                   // Ctrl-C
{"type":"eof"}                                         // Ctrl-D
{"type":"kill"}                                        // SIGKILL the child
```

### Matcher shape

`session.wait` takes a tagged `Matcher`. Unit variants omit `value`; struct/tuple variants include it.

```json
{"type":"contains_text","value":"Do you want to proceed?"}
{"type":"screen_regex","value":"\\bready\\b"}
{"type":"transcript_contains","value":"error"}
{"type":"transcript_regex","value":"^panic"}
{"type":"cursor_at","value":{"row":1,"col":0}}
{"type":"screen_stable","value":{"min_ms":250}}
{"type":"process_exited"}
{"type":"any","value":[ <matcher>, ... ]}
{"type":"all","value":[ <matcher>, ... ]}
```

Every successful wait returns evidence: the `sequence` number observed at decision time, the resulting `snapshot`, and a `transcript_tail`. Use that to drive the next turn deterministically — never `sleep` and re-poll.

### Notifications

Notifications are **opt-in per connection** via `server.set_notifications {enabled: true}`:

- `session.changed` — emitted with the latest coalesced `sequence` after PTY output.
- `session.exited` — emitted once the child's lifecycle reports exit.

Responses to a request are always written before any queued notification, so an NDJSON consumer can read line-by-line and still get a deterministic stream.

## Snapshot Shape

`session.snapshot` / `session.wait` return a rich `ScreenSnapshot`:

```json
{
  "size": {"rows":24,"cols":80,"pixel_width":0,"pixel_height":0},
  "cursor": {"row":1,"col":7,"visible":true},
  "sequence": 12,
  "plain_text": "...\nready",
  "cells": [ {"row":1,"col":0,"text":"r","fg":..., "bg":..., "style":...}, ... ],
  "alternate_screen": false,
  "application_cursor": false,
  "application_keypad": false,
  "title": "claude"
}
```

`plain_text` is the simplest matcher surface; `cells` carries fg/bg/style/wide-char flags for fidelity-sensitive UIs.

## Redaction

Reads default to **redacted**: built-in patterns mask `token=`, `password=`, `Authorization:` bearer headers, common API-key keys, etc. Override per call:

```json
{"redact": false}                                   // raw — only do this in trusted local debugging
{"redaction": {
   "enabled": true,
   "replacement": "[X]",
   "extra_regexes": ["internal-[0-9]+"]
}}
```

The CLI also redacts fatal error messages and ptywright-owned `tracing` events. Raw transcript file streaming (`raw_transcript_path`) is **not** redacted — treat the file as sensitive.

## Runtime Directory and Logging

ptywright keeps state under `~/.ptywright/` (override with `PTYWRIGHT_HOME`):

```
~/.ptywright/
├── config.toml           # see config.example.toml in repo root
└── logs/
    └── ptywright.YYYY-MM-DD.log
```

| Env var | Purpose |
| --- | --- |
| `PTYWRIGHT_HOME` | Override the entire runtime root. Tests/CI should pin this to a tempdir. |
| `PTYWRIGHT_LOG` | `tracing-subscriber` `EnvFilter` directive. Overrides `[logging].level` from config. e.g. `info,ptywright::rpc=debug`. |

`config.toml` keys (all optional):

```toml
[logging]
level    = "warn"     # EnvFilter directive
file     = true       # write rotated log files to <home>/logs/
max_days = 14         # retention; 0 disables
format   = "text"     # "text" or "json"
```

Per-mode logging sinks:

| Mode | File | Stderr | Stdout |
| --- | --- | --- | --- |
| `run` | yes | no | child PTY bytes only |
| `serve --stdio` | yes | yes | JSON-RPC only (never logs) |
| `serve --socket` | yes | yes | (none) |
| `completions`, `--help`, `--version` | no | minimal | command output |

## Agent Patterns

### Round-trip `server.capabilities`

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}' \
  | ptywright serve --stdio | jq '.result | {version, methods: (.methods | length)}'
```

### Spawn a shell, wait for output, snapshot

```bash
{
  echo '{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"program":"/bin/sh","args":["-lc","printf ready && sleep 5"]}}'
  echo '{"jsonrpc":"2.0","id":2,"method":"session.wait","params":{"session":"s1","matcher":{"type":"contains_text","value":"ready"},"timeout_ms":5000}}'
  echo '{"jsonrpc":"2.0","id":3,"method":"session.snapshot","params":{"session":"s1"}}'
} | ptywright serve --stdio | jq -c '{id, result_keys: (.result // {} | keys)}'
```

Substitute the real session id (returned by `session.create`) into id 2 and 3 — the example uses `s1` for brevity but the server allocates ids.

### Drive Claude Code end-to-end

The Claude Code plugin is the primary thing we exercise, so this is the pattern to reach for first. Every `adapter.*` response carries:

```json
{
  "adapter": "e1",
  "plugin": "claude-code",
  "state": {
    "state": "waiting_for_permission",
    "confidence": 0.87,
    "evidence": "matched 'Do you want to proceed?' with screen_stable 300ms",
    "sequence": 42
  }
}
```

State names are `snake_case`: `starting`, `ready`, `prompt_submitted`, `thinking`, `waiting_for_permission`, `waiting_for_plan_approval`, `waiting_for_trust`, `waiting_for_user_input`, `completed_turn`, `cancelling`, `exited`, `error`, `plugin_error`.

Run this Python driver against a single stdio server. It uses the generic `adapter.*` surface, captures the real adapter id, loops on `adapter.wait`, branches on the state, handles the workspace-trust dialog explicitly, and gives up cleanly on `error` / `plugin_error`:

```python
# drive_claude.py — adapter.* driver with trust + permission + plan-approval handling
import json, subprocess, sys

PLUGIN = "claude-code"
WAIT_MS = 120_000
APPROVE = {"waiting_for_permission", "waiting_for_plan_approval"}
TRUST = "waiting_for_trust"
TERMINAL = {"completed_turn", "exited", "error", "plugin_error"}

proc = subprocess.Popen(
    ["ptywright", "serve", "--stdio"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
)

def rpc(rid, method, params=None):
    req = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}
    proc.stdin.write(json.dumps(req) + "\n"); proc.stdin.flush()
    return json.loads(proc.stdout.readline())

start = rpc(1, "adapter.start", {"plugin": PLUGIN, "cwd": "/path/to/repo"})
aid = start["result"]["adapter"]
print("started:", start["result"]["state"]["state"])

rpc(2, "adapter.send", {"adapter": aid, "intent": "send_prompt",
                        "params": {"prompt": "summarize CHANGELOG.md"}})

turn = 0
while True:
    turn += 1
    resp = rpc(10 + turn, "adapter.wait", {"adapter": aid, "timeout_ms": WAIT_MS})
    snap = resp["result"]["state"]
    print(f"turn {turn}: {snap['state']} (seq={snap['sequence']}, conf={snap['confidence']:.2f})")
    if snap["state"] in TERMINAL:
        break
    if snap["state"] == TRUST:
        rpc(100 + turn, "adapter.send", {"adapter": aid, "intent": "approve_trust", "params": {}})
    elif snap["state"] in APPROVE:
        rpc(100 + turn, "adapter.send", {"adapter": aid, "intent": "approve", "params": {}})

print("evidence:", snap["evidence"])
rpc(999, "adapter.close", {"adapter": aid})
proc.stdin.close()
proc.wait(timeout=5)
sys.exit(0 if snap["state"] == "completed_turn" else 1)
```

Run it with `python3 drive_claude.py` from any cwd — the script handles ids, framing, and state branching for you. Swap `approve` for `deny` to exercise plan rejection, or send `intent: "cancel"` mid-loop for mid-turn cancellation.

#### Fallback: drive via `session.*` for raw transcript control

When you need the raw PTY transcript rather than the plugin's classified turn, drive Claude Code through generic `session.*` primitives. This trades the plugin's automated turn classification for a hand-rolled completion matcher and gives you full transcript access in exchange. The plugin classifier no longer false-positives on the idle screen (see the body/status split note above), so this path is a deliberate choice rather than a workaround.

```python
# drive_claude_session.py — drive Claude through session.* for raw transcript access
import json, os, subprocess
proc = subprocess.Popen(
    ["ptywright", "serve", "--stdio"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    text=True, bufsize=1, env={**os.environ, "PTYWRIGHT_LOG": "warn"},
)
def rpc(rid, method, params=None):
    proc.stdin.write(json.dumps({"jsonrpc":"2.0","id":rid,"method":method,"params":params or {}})+"\n")
    proc.stdin.flush()
    return json.loads(proc.stdout.readline())["result"]

rpc(0, "server.capabilities")
sid = rpc(1, "session.create", {
    "program": "claude",
    "args": ["--model", "haiku", "--permission-mode", "bypassPermissions"],
    "cwd": "/path/to/repo", "rows": 48, "cols": 160,
    "transcript_max_chars": 524288,
})["session"]

# Idle prompt: `❯` visible + screen stable.
rpc(2, "session.wait", {"session":sid,"matcher":{"type":"all","value":[
    {"type":"contains_text","value":"❯"},
    {"type":"screen_stable","value":{"min_ms":800}}]},"timeout_ms":20000})

rpc(3, "session.input", {"session":sid,"action":{"type":"paste","value":"What is ptywright? One sentence."}})
rpc(4, "session.input", {"session":sid,"action":{"type":"key","value":"enter"}})

# Turn complete: `⏺` (answer bullet) on screen, then settle.
rpc(5, "session.wait", {"session":sid,"matcher":{"type":"contains_text","value":"⏺"},"timeout_ms":120000})
rpc(6, "session.wait", {"session":sid,"matcher":{"type":"screen_stable","value":{"min_ms":1500}},"timeout_ms":15000})

print(rpc(7, "session.snapshot", {"session":sid,"redact":False})["plain_text"])
print(rpc(8, "session.transcript", {"session":sid,"redact":False})["text"])
rpc(99, "session.kill", {"session":sid})
```

Why this matcher pair? `⏺` is Claude's answer-block bullet — it only renders when Claude has produced its response. Pinning to specific completion verbs is brittle: Claude Code 2.1.x rotates the verb per turn (`Cogitated for 3s`, `Churned for 11s`, `Baked for 9s`, `Unravelling…`). The bullet + stable-screen pair is verb-agnostic and held across the Claude Code 2.0 and 2.1 versions we've exercised.

For the `--permission-mode bypassPermissions` flag to take effect on first launch in a new directory, the workspace must already be trusted — Claude Code only auto-skips the trust dialog in `--print` mode. Reuse a directory you've previously approved interactively, or drive the trust prompt explicitly via `session.input` actions (or use the `adapter.*` driver above, which handles `waiting_for_trust` for you).

### Claude Code plugin test matrix

These are the scenarios worth driving repeatedly while the plugin is still hardening. Use the script above as a base and tweak the prompt / branching.

| Scenario | Setup | Expected terminal state |
| --- | --- | --- |
| Smoke (start + state) | `adapter.start {plugin: "claude-code"}` → `adapter.state` → `adapter.send {intent: "cancel"}` | starts as `starting`/`ready`, ends with cancel returning a state |
| Happy path | Prompt that needs no tool approval | `completed_turn` after one or more `thinking` rounds |
| Workspace trust | First-launch directory; expect `waiting_for_trust` → `adapter.send {intent: "approve_trust"}` → continues into normal turn flow | numeric `1`+Enter accepted, no infinite re-prompt |
| Permission approve | Prompt that triggers `Bash`/`Edit` permission UI | `waiting_for_permission` → `approve` → `completed_turn` |
| Permission deny | Same setup, call `adapter.send {intent: "deny"}` | adapter recovers to `ready` or returns `completed_turn` with denial evidence |
| Plan approve | Prompt that triggers plan mode | `waiting_for_plan_approval` → `approve` → `thinking` → `completed_turn` |
| Mid-turn cancel | After `adapter.wait` returns `thinking`, call `adapter.send {intent: "cancel"}` | transitions through `cancelling`, ends with stable state. `cancelling` is reported until `adapter.wait` returns with a stable-enough screen; `adapter.state` polling alone will stay on `cancelling` until the next mutating intent. |
| Crash recovery | `adapter.start` with a bogus `program` | `error` / `plugin_error` returned with evidence; subsequent calls reject the dead adapter |
| Long turn | Prompt that takes >60s; loop `adapter.wait` with `timeout_ms: 30000` | repeated `thinking` until `completed_turn`; no spurious `completed_turn` from premature stable-screen |

Two things to verify on every run:

1. **`screen_stable` evidence is present in `completed_turn`.** Without it the turn-boundary detection is just string matching. The evidence string from the state snapshot should mention `screen_stable` and a duration ≥ 300 ms.
2. **No protocol noise on stdout.** Capture stderr separately (`2>err.log`) and assert that every stdout line is valid JSON-RPC. Any human-readable banner is a regression in the per-mode logging wiring.

### Fixture-driven plugin tests

The repo ships sanitized Claude Code screen fixtures under `tests/fixtures/claude_code/`. When upstream Claude Code changes its TUI (new banner, renamed permission prompt, different plan UI), update the fixture and the classifier in one PR — `plugins/claude-code/main.lua` and `tests/fixtures/claude_code/` belong together. If a real session classifies the wrong state, capture the screen to a new fixture file before fixing the Lua, so the regression is locked in.

### Wait until the screen stops changing

```json
{"type":"screen_stable","value":{"min_ms":500}}
```

Combine with a content matcher to require both evidence kinds:

```json
{"type":"all","value":[
  {"type":"contains_text","value":"Approve"},
  {"type":"screen_stable","value":{"min_ms":250}}
]}
```

### Local socket, persistent state

```bash
ptywright serve --socket /tmp/ptywright.sock &
nc -U /tmp/ptywright.sock <<<'{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}'
```

Multiple `nc -U` clients can talk to the same server. Only `session.*` ids are shared across connections; `adapter.*` ids are per-connection — drive them from the same socket client that called `adapter.start`. Use a process supervisor in production; the binary itself does not daemonize.

### Force a clean test environment

```bash
PTYWRIGHT_HOME=$(mktemp -d) PTYWRIGHT_LOG=info ptywright serve --stdio
```

Useful in CI and inside this skill's own test commands — nothing lands in the user's `~/.ptywright/`.

## Key Invariants for Agents

- **Never write to stdout in `serve --stdio` mode.** Stdout is JSON-RPC framing. Use stderr or the log file.
- **`run` is debug-only.** Its stdout is raw terminal bytes; do not pipe through `jq`.
- **Waits return evidence; do not sleep.** Every successful `session.wait` ships back `sequence`, `snapshot`, and `transcript_tail`. The claude-code plugin's turn detection further requires a `screen_stable` window — copy that pattern for any flaky TUI.
- **Reads are redacted by default.** Pass `{"redact": false}` only for local debugging in trusted contexts.
- **Transcripts are bounded.** Default 128 KiB per session. Bump via `session.create.transcript_max_chars` or stream raw bytes to a file with `raw_transcript_path`.
- **Notifications are opt-in per connection.** Call `server.set_notifications` once after connecting if you want `session.changed` / `session.exited`.
- **Single binary.** No Python/Node sidecar; the embedded Lua plugin is compiled in.
- **Cross-platform target.** macOS/Linux PTYs and Unix sockets, Windows ConPTY and named pipes. Some flags (e.g. socket path syntax) differ — the help text is the source of truth.

## Troubleshooting

- **"serve requires --stdio or --socket"** — pick one transport; the two are mutually exclusive.
- **"refusing to replace non-socket path"** — the path you passed to `--socket` exists but isn't a Unix socket. Move or delete it.
- **JSON-RPC errors with code `-32602`** — `InvalidParams`. Re-check the param schemas above; missing `session` ids or unknown adapter ids are the most common causes.
- **Code `-32001`** — wait timed out. The response still includes the snapshot/transcript_tail at decision time; use that to diagnose what the screen actually shows.
- **Code `-32002`** — session closed mid-call. Re-create with `session.create`.
- **Logs missing on `ptywright run`** — by design, `run` writes only to `~/.ptywright/logs/`. Tail that file instead of expecting stderr output.

## Updating This Skill

This skill ships in the ptywright repository. To pull the latest version from `main`:

```bash
# Source repository
https://github.com/utensils/ptywright

# Skill file location within the repo
.claude/skills/ptywright/SKILL.md

# Fetch the latest skill directly
curl -sL https://raw.githubusercontent.com/utensils/ptywright/main/.claude/skills/ptywright/SKILL.md \
  -o ~/.claude/skills/ptywright/SKILL.md
```

When copying this skill to other workspaces, always pull from `main` so the JSON-RPC method list, action/matcher tags, and Claude adapter state names match the binary you're driving.
