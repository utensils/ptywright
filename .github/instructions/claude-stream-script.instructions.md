---
applyTo: "scripts/claude-stream.py,tests/scripts/**/*.py"
---

# claude-stream — review constraints

`scripts/claude-stream.py` is the **single-turn** Python wrapper that demonstrates every primitive a real consumer would touch: `adapter.start`, `adapter.send`, `adapter.state`, `adapter.inspect`, `adapter.transcript`, `adapter.wait`, `adapter.close`, plus the `session.output` / `session.exited` notification subscription. Most of the file is comments documenting WHY each guard exists; the actual control flow is short.

## Hard rules

1. **Stdlib only.** No pytest, no rich, no aiohttp. The test suite (`tests/scripts/test_claude_stream.py`) uses `unittest`. The script itself uses only `argparse`, `subprocess`, `threading`, `queue`, `json`, `re`, `time`, `signal`, `os`, `sys`, `pathlib`, `tempfile`. Don't propose dependencies.
2. **Single-turn.** The script submits one prompt and streams the result. Don't propose interactive REPL features, multi-turn loops, or session persistence. Those exist as `ptywright repl` (Rust-side).
3. **Screen-evidence driven.** Control flow is gated on classifier state transitions and `session.exited` notifications. The ONE acknowledged timing exception is the bounded 3 s settle wait in `submit()`, capped, and skipped when `input_prompt_already_observed=True`. Do not propose adding "wait N polls" guards, "if elapsed > X" branches, or byte-count thresholds for completion verification.
4. **Two layered fallbacks at completion.** Transcript-based dump runs FIRST and ALWAYS (deduped via `printed_lines`). Visible-body dump runs only when transcript dump returned False AND body has grown. Do not collapse these or reorder them — the transcript is the canonical record for content that scrolled past the visible body.
5. **Activity ticker scans full screen + bytes/sec.** `_screen_for_activity(ins)` unions `body_text` + `status_text`. The bytes-rate sampler shows `+N.N KB/s` so frozen-body subagent phases still display "Claude is alive". Don't strip these signals.

## TUI/transcript shapes the script cares about

- `_CHROME_SPINNER_GLYPHS = "✶✻✺✦·•⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⏺✽✳✢⏵"` — the Python char class is a SUPERSET of the Lua `SPINNER_GLYPHS` table. Lua tracks the classifier-relevant subset; Python needs broader coverage to filter streaming output. Test `test_python_chrome_set_covers_lua_spinner_set` pins this superset relationship.
- `_CHROME_SEARCHED_RE` matches ONLY in-flight forms (verb + `…` + `(ctrl+o to expand)`). Completed summaries like `Read 5,103 lines (ctrl+o to expand)` (past tense, no ellipsis) are substantive content and must pass through.
- `_ACTIVITY_TOOL_BULLET_RE = r"^⏺\s+\S+\(.+\)\s*$"` matches `⏺ Read(file)` / `⏺ Bash(cmd)` — completed tool announcements for the ticker.

## Prompt-row recognition

- `❯ <text>` — prompt with text (submitted echo OR post-turn ghost suggestion).
- Bare `❯` / `>` (with or without NBSP padding) — idle prompt.
- **NOT a prompt row:** `> <text>` — Markdown blockquote in answer prose. `_is_prompt_with_text` rejects this shape.
- The transcript fallback's prompt-echo skip is restricted to `❯ <text>` so Markdown blockquotes flow through to output.
- The live diff path skips lines starting with `❯` to filter post-turn ghost suggestions.

## Hard rule: `thinking` is NOT a submit-ready state

`SUBMIT_READY_STATES` explicitly excludes `thinking`. Calling `send_prompt` while a prior turn is in flight flips `last_intent` back to `prompt_submitted` and confuses the classifier. The plugin exposes `steer` (same paste bytes, no intent flip) for legitimate mid-turn injection — but claude-stream is single-turn and polls until `waiting_for_user_input` / `completed_turn`.

## Review anti-patterns

- **"Add `--retry-on-timeout`"** — single-turn, no retry. Failures exit non-zero.
- **"Increase `PASTE_REACTION_BYTES` to 1000"** — it's deliberately 256 (above idle status-bar noise, below first-poll real-reaction). Higher values starve the script of legitimate first-tick acknowledgements.
- **"Add a `--wait-for-content N` flag"** — content-threshold timing heuristic. Use the transcript fallback's canonical-record approach instead.
- **"Cache transcripts in-memory between polls"** — the wire size is small; reading transcript on demand is fine. Caching creates a sync-vs-fresh question we don't need.
- **"Convert `_dump_answer_region` to a generator"** — fine in principle but breaks the test surface that uses `mock.patch.object(sys, "stdout", ...)`. The procedural form is intentional.

## When adding tests

- Use stdlib `unittest`. Tests live in `tests/scripts/test_claude_stream.py`.
- The script is imported via `importlib.util` because its filename has a hyphen — don't rename the script.
- Fake transports for JSON-RPC framing tests use OS pipes; the synchronisation barrier is `in_r_file.readline()` (blocks until the client has written its request, by which point the response queue is registered). Do not pre-stage responses on stdout — that races the reader thread.
