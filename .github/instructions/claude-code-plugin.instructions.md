---
applyTo: "plugins/claude-code/**,tests/lua_classifier_tests.rs,tests/lua_plugin_intents.rs"
---

# Claude Code plugin — review constraints

`plugins/claude-code/main.lua` owns ALL Claude Code TUI knowledge: state names, intent names, spinner glyphs, marker shapes, screen geometry, screen-element layouts. The Rust core knows nothing about Claude beyond "embed this manifest and Lua source".

## TUI shapes Claude Code 2.1.x renders

Detector regressions usually come from misunderstanding one of these. Keep this list authoritative.

### Spinner status line — `has_thinking_spinner_line` / `has_spinner_ellipsis_shape`
- Bare form: `✻ Thinking…`
- **Counter form (Sonnet 4.6 default):** `✻ Thinking… (12s · ↓ 339 tokens · thinking)`
- The ellipsis can be MID-line followed by a parenthesized counter. Patterns that require `…` at literal line end miss the counter form entirely — this caused intermittent "no active work indicator" misclassifications and silent thinking phases.
- Glyph set: `SPINNER_GLYPHS = ✶ ✻ ✺ ✦ · • ⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏`. `⏺` is the tool-bullet glyph (NOT spinner) and the marker glyph `✻` IS in the spinner set — that's why the marker is distinguished by its `for <N>s` tail.

### Tool-progress collapsible row — `has_collapsible_tool_progress_row`
- In-flight: `⏺ Reading 2 files… (ctrl+o to expand)` — `⏺` start, `…` somewhere, `(ctrl+o to expand)` end.
- Completed summary: `⏺ Read(plugins/claude-code/main.lua)` (no ellipsis, no expand hint) — this is substantive content, NOT chrome.
- `Read 5,103 lines (ctrl+o to expand)` (past tense, no leading `⏺`) — completed result line, substantive content.

### Turn completion marker — `has_turn_completion_marker`
- Shape: `✻ <Verb> for <N>s` (e.g. `Brewed for 1s`, `Worked for 5s`, `Churned for 1m 44s`).
- Verb is past-tense; spinner shows present-progressive form (`Brewing`, `Working`). Tail is `for <digit>`.
- The marker must satisfy THREE structural checks: (a) trailing-chrome only after it, (b) substantive content above it (excluding prompt echoes and progress chrome), and (c) caller's state must be `prompt_submitted`.

### Input prompt — `has_input_prompt`
- `❯ <text>` — input box with text (submitted-prompt echo OR post-turn ghost suggestion).
- `❯` (bare) — idle prompt. May be padded with NBSP (`\194\160`).
- `>` (bare) / ` >` — older / ASCII fallback idle prompt. NBSP-padded forms also accepted.
- **NOT a prompt row:** `> <text>` — that's a Markdown blockquote in answer prose. Restrict prompt-with-text recognition to `❯`.

### Status bar (bottom 3 rows, excluded from `body_text`)
- `<user> @ <host>  /path  [<Model> <Version>]`
- `⏵⏵ auto mode on (shift+tab to cycle) · ← for agents`
- Status-bar shape detection (`is_post_marker_trailing_line`) is generic by design — recognises model bracket, user@host+bracket, or `⏵⏵` glyph. **Never** anchor on the fixture's sanitized literal `user ` prefix.

### Dialogs
- **Workspace trust:** `Do you trust the files in this folder?` + `1. Yes, proceed` / `2. No, exit`. Requires numeric input (1 + Enter), not bare Enter.
- **Permission:** `Do you want to proceed?` + focused `❯ <digit>. ...` OR `[Enter]` + `[Esc]` on same line.
- **Plan approval:** `plan` header + numbered list (`\n1[.)]`).
- **Model picker:** header phrase ending in `:` + focused `❯ <digit>.` + nav-hint line. ALL THREE anchors required.
- **Login:** title phrase + one of {action prompt, line-anchored OAuth URL, `ANTHROPIC_API_KEY` env var}. Login URL regex uses `[\w./\-_?=&%]*` (zero or more) so root-domain URLs like `https://claude.ai/login` match.

## Geometry presets

The plugin manifest declares `default_target` with `rows = 60, cols = 200`. This is wide enough that:
- The status bar (3 rows) and `❯` / `>` prompt glyphs never wrap.
- The "Total cost:" usage screen renders complete.
- Tool-call status lines fit on one line.

**Smaller geometries (e.g. 40×120) cause line wrapping that breaks classification.** If tests need a smaller size, they should test only behaviors that survive wrapping; the production preset is 60×200.

## Hard rules

1. **The classifier is stateless.** Plugin state across `M.classify` calls would require host changes. Don't propose caching previous-frame info or "marker counter" tracking in the plugin without acknowledging the Rust host work needed.
2. **Multi-turn stale-marker is a known limitation.** If a prior turn's `✻ X for N` is still on screen when the next prompt submits, the poll-path can fire `completed_turn` against the new turn. claude-stream is single-turn so it's not exercised — proposed fixes need plugin-state-across-calls and should be flagged as a separate PR.
3. **Use `screen_text` (full screen) not `body_text` for active-work detection.** Spinner / token-counter often renders in the status bar during subagent runs. The body-only restriction was a misguided optimization; status-bar rows have a known structural shape and don't match any active-work pattern.
4. **Use `screen` (full screen) not `body` for `has_input_prompt` in completion gates.** Real Claude screens render `separator + ❯ + 2 status rows` at the bottom where `body_text` strips the prompt row.
5. **TDD for classifier changes.** Add a fixture under `plugins/claude-code/fixtures/<name>.txt` + sibling `.expected.json`. The matrix auto-discovers it. Hand-edit fixtures rather than capturing fresh ones if the change is structural (you'll edit a real capture to isolate the variation you're testing).
6. **Wait-matcher anchors mirror classifier states.** Every classifier-recognised non-`starting`/`thinking` state needs a corresponding anchor in `wait_turn_matcher` so callers using `adapter.wait` wake at the same time `adapter.state` would report the state. Login, model-picker, error banners, dialogs all need anchors.
7. **No prose anchors.** If your detector matches a phrase that could appear in Claude's answer text (`"permission denied"`, `"thinking about"`, `"reading file"`), the detector will false-positive on turns whose answer summarises that phrase. Use structural shape — glyph position + line layout + adjacency — instead of single-substring matching.

## Review anti-patterns

- **"Tighten the regex to `^.*…$`"** — misses the counter form. See spinner shapes above.
- **"Match prompts as `^[>❯]`"** — allows Markdown blockquotes. See input-prompt shapes above.
- **"Lower the row count to 40"** — breaks unwrapping invariants. Production preset is 60×200.
- **"Combine `is_post_marker_trailing_line` with `is_prompt_echo_line`"** — those serve different purposes. Trailing-line check accepts chrome BELOW the marker. Prompt-echo line check identifies the user's submission to anchor the substance scan.
- **"Use `string.match` with one big pattern"** — Lua patterns are not regex; balanced-string matching (`%b()`) misbehaves on multi-byte glyphs. Prefer explicit character checks and substring searches over clever single-pattern matches.
