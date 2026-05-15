---
name: ptywright
description: Drive interactive terminal applications through PTYs using the ptywright CLI. Use when asked to spawn or attach to a TUI, capture terminal screen state, send key sequences or text into a PTY, wait for prompts or screen-stable evidence, automate Claude Code interactively (start, send a prompt, approve/deny, wait for turn boundaries), or run ptywright's JSON-RPC server over stdio or a local socket.
argument-hint: [subcommand or question]
allowed-tools: Bash, Read, Glob, Grep
---

# ptywright — Headless PTY/TUI Automation CLI

ptywright spawns interactive terminal programs in a real PTY, parses their screen state with a `vt100`-backed engine, sends input as deterministic actions, and waits on screen/transcript/lifecycle matchers. It exposes a JSON-RPC 2.0 server over stdio or a local socket so agents can drive any TUI — and ships a Lua-backed Claude Code adapter on top of the same primitives.

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
- If the user mentions **Claude Code**, prefer the `claude.*` adapter methods over rebuilding turn detection from `session.*` primitives. See the [Claude Code adapter pattern](#drive-claude-code-end-to-end) and [test matrix](#claude-code-adapter-test-matrix) — this is the most-exercised path in the project today.
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

Multiple clients can connect concurrently and share session/Claude-adapter state. Each connection has its own framing/subscription state.

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
              "claude.start", "claude.send_prompt", "claude.wait_turn",
              "claude.approve", "claude.deny", "claude.cancel", "claude.state",
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

#### `claude.*` — Claude Code adapter (Lua-backed)

| Method | Params | Returns |
| --- | --- | --- |
| `claude.start` | `{program?="claude", args?, cwd?, env?, rows?=40, cols?=120}` | `{claude, state}` |
| `claude.send_prompt` | `{claude, prompt}` | `{state}` |
| `claude.wait_turn` | `{claude, timeout_ms?}` | `{state}` (classification + evidence) |
| `claude.approve` | `{claude}` | `{approved: true}` — no state; re-query `claude.state` to inspect |
| `claude.deny` | `{claude}` | `{denied: true}` — no state; re-query `claude.state` to inspect |
| `claude.cancel` | `{claude}` | `{state}` |
| `claude.state` | `{claude}` | `{state}` |

The adapter classifies states (`Starting`, `Ready`, `Thinking`, `WaitingForPermission`, `WaitingForPlanApproval`, `CompletedTurn`, etc.) using **stable-screen evidence** rather than raw string matches — that's the whole point of going through Lua plus the screen engine.

> **Known classifier issue (Claude Code 2.1.142, May 2026):** the built-in Lua plugin reports `waiting_for_permission` at confidence ~0.84 on the *idle* input screen because the status-bar string `⏵⏵ bypass permissions on (shift+tab to cycle)` contains the substring `permissions`. Until `plugins/claude-code/main.lua` is taught to exclude status-bar lines, automated drivers that branch on `waiting_for_permission` against current Claude Code releases will misfire. Workaround: drive Claude Code through the generic `session.*` primitives (see the [end-to-end pattern](#drive-claude-code-end-to-end)) and use `⏺` (answer-bullet) + `screen_stable` as the turn-end signal. Capture a fixture under `tests/fixtures/claude_code/` before fixing.

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

The Claude adapter is the primary thing we exercise, so this is the pattern to reach for first. Every `claude.*` response carries:

```json
{
  "claude": "c1",
  "state": {
    "state": "waiting_for_permission",
    "confidence": 0.87,
    "evidence": "matched 'Do you want to proceed?' with screen_stable 300ms",
    "sequence": 42
  }
}
```

State names are `snake_case`: `starting`, `ready`, `prompt_submitted`, `thinking`, `waiting_for_permission`, `waiting_for_plan_approval`, `waiting_for_user_input`, `completed_turn`, `cancelling`, `exited`, `error`, `plugin_error`. There is **no** `claude.transcript` method — the adapter owns its session internally. Inspect `state.evidence` for the latest screen excerpt, or drive Claude Code through generic `session.*` calls if you need the raw transcript.

Run this Python driver against a single stdio server. It captures the real `claude` id, loops on `wait_turn`, branches on the state, and gives up cleanly on `error` / `plugin_error`:

```python
# drive_claude.py — minimal happy-path + permission/plan-approval handler
import json, subprocess, sys

WAIT_MS = 120_000
APPROVE = {"waiting_for_permission", "waiting_for_plan_approval"}
TERMINAL = {"completed_turn", "exited", "error", "plugin_error"}

proc = subprocess.Popen(
    ["ptywright", "serve", "--stdio"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
)

def rpc(rid, method, params=None):
    req = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}
    proc.stdin.write(json.dumps(req) + "\n"); proc.stdin.flush()
    return json.loads(proc.stdout.readline())

start = rpc(1, "claude.start", {"cwd": "/path/to/repo"})  # spawns `claude`
cid = start["result"]["claude"]
print("started:", start["result"]["state"]["state"])

rpc(2, "claude.send_prompt", {"claude": cid, "prompt": "summarize CHANGELOG.md"})

turn = 0
while True:
    turn += 1
    resp = rpc(10 + turn, "claude.wait_turn", {"claude": cid, "timeout_ms": WAIT_MS})
    snap = resp["result"]["state"]
    print(f"turn {turn}: {snap['state']} (seq={snap['sequence']}, conf={snap['confidence']:.2f})")
    if snap["state"] in TERMINAL:
        break
    if snap["state"] in APPROVE:
        rpc(100 + turn, "claude.approve", {"claude": cid})

print("evidence:", snap["evidence"])
proc.stdin.close()
proc.wait(timeout=5)
sys.exit(0 if snap["state"] == "completed_turn" else 1)
```

Run it with `python3 drive_claude.py` from any cwd — the script handles ids, framing, and state branching for you. Swap in `claude.deny` for plan rejection tests, or call `claude.cancel` before the loop ends to exercise mid-turn cancellation.

#### Workaround: drive via `session.*` until the classifier is fixed

While the `waiting_for_permission` false-positive is unresolved (see above), the most reliable way to drive Claude Code is through generic PTY primitives. This trades the adapter's automated turn classification for a hand-rolled completion matcher — and gives you full transcript access in exchange.

```python
# drive_claude_session.py — bypass the classifier, drive Claude through session.*
import json, os, subprocess, time
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
rpc(99, "session.kill", {"session":sid})
```

Why this matcher pair? `⏺` is Claude's answer-block bullet — it only renders when Claude has produced its response. Pinning to specific completion verbs is brittle: Claude Code 2.1.x rotates the verb per turn (`Cogitated for 3s`, `Churned for 11s`, `Baked for 9s`, `Unravelling…`). The bullet + stable-screen pair is verb-agnostic and held across the Claude Code 2.0 and 2.1 versions we've exercised.

For the `--permission-mode bypassPermissions` flag to take effect on first launch in a new directory, the workspace must already be trusted — Claude Code only auto-skips the trust dialog in `--print` mode. Reuse a directory you've previously approved interactively, or drive the trust prompt explicitly via `session.input` actions.

### Claude Code adapter test matrix

These are the scenarios worth driving repeatedly while the adapter and built-in Lua plugin are still hardening. Use the script above as a base and tweak the prompt / branching:

| Scenario | Setup | Expected terminal state |
| --- | --- | --- |
| Smoke (start + state) | `claude.start` → `claude.state` → `claude.cancel` | starts as `starting`/`ready`, ends with cancel returning a state |
| Happy path | Prompt that needs no tool approval | `completed_turn` after one or more `thinking` rounds |
| Permission approve | Prompt that triggers `Bash`/`Edit` permission UI | `waiting_for_permission` → `approve` → `completed_turn` |
| Permission deny | Same setup, call `claude.deny` | adapter recovers to `ready` or returns `completed_turn` with denial evidence |
| Plan approve | Prompt that triggers plan mode | `waiting_for_plan_approval` → `approve` → `thinking` → `completed_turn` |
| Mid-turn cancel | After `wait_turn` returns `thinking`, call `claude.cancel` | transitions through `cancelling`, ends with stable state |
| Crash recovery | `claude.start` with a bogus `program` | `error` / `plugin_error` returned with evidence; subsequent calls reject the dead adapter |
| Long turn | Prompt that takes >60s; loop `wait_turn` with `timeout_ms: 30000` | repeated `thinking` until `completed_turn`; no spurious `completed_turn` from premature stable-screen |

Two things to verify on every run:

1. **`screen_stable` evidence is present in `completed_turn`.** Without it the turn-boundary detection is just string matching. The evidence string from the state snapshot should mention `screen_stable` and a duration ≥ 300 ms.
2. **No protocol noise on stdout.** Capture stderr separately (`2>err.log`) and assert that every stdout line is valid JSON-RPC. Any human-readable banner is a regression in the per-mode logging wiring.

### Fixture-driven adapter tests

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

Multiple `nc -U` clients can talk to the same server; session/Claude-adapter state is shared. Use a process supervisor in production; the binary itself does not daemonize.

### Force a clean test environment

```bash
PTYWRIGHT_HOME=$(mktemp -d) PTYWRIGHT_LOG=info ptywright serve --stdio
```

Useful in CI and inside this skill's own test commands — nothing lands in the user's `~/.ptywright/`.

## Key Invariants for Agents

- **Never write to stdout in `serve --stdio` mode.** Stdout is JSON-RPC framing. Use stderr or the log file.
- **`run` is debug-only.** Its stdout is raw terminal bytes; do not pipe through `jq`.
- **Waits return evidence; do not sleep.** Every successful `session.wait` ships back `sequence`, `snapshot`, and `transcript_tail`. The Claude adapter's turn detection further requires a `screen_stable` window — copy that pattern for any flaky TUI.
- **Reads are redacted by default.** Pass `{"redact": false}` only for local debugging in trusted contexts.
- **Transcripts are bounded.** Default 128 KiB per session. Bump via `session.create.transcript_max_chars` or stream raw bytes to a file with `raw_transcript_path`.
- **Notifications are opt-in per connection.** Call `server.set_notifications` once after connecting if you want `session.changed` / `session.exited`.
- **Single binary.** No Python/Node sidecar; the embedded Lua adapter is compiled in.
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
