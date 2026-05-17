# GitHub Copilot — repository instructions

ptywright is a Rust CLI/library for driving interactive terminal applications through PTYs. The Rust core is **strictly generic**; application-specific behavior (Claude Code, future TUIs) lives entirely in Lua plugins under `plugins/<name>/`. The canonical contributor guide is `AGENTS.md` (symlinked as `CLAUDE.md`) — read it before suggesting structural changes.

## Hard rules that override review heuristics

These reflect concrete past incidents. Suggestions that violate them are regressions, not improvements.

1. **No application-specific identifiers in `src/`.** State names, intent names, fixture conventions, and screen-glyph constants for Claude Code (or any future plugin) belong in `plugins/<name>/main.lua` and the single `BUILTIN_PLUGINS` entry in `src/plugin.rs`. Do not propose moving plugin logic into Rust modules or adding per-plugin RPC namespaces.
2. **Screen-evidence-driven detection over timing thresholds.** Prefer structural shape matching (regex, glyph sets, line layouts, marker positions) over byte-count thresholds, settle delays, debounce windows, or "wait N polls before trusting" guards. The user has explicitly rejected timing heuristics. The bounded 3 s settle in `claude-stream`'s `submit()` is the ONE acknowledged exception, and it is skipped when prior evidence justifies it.
3. **Be vigilant against breaking working state.** This codebase has a working baseline that real users run. Suggest the minimum diff that fixes the stated problem; do not bundle refactors, naming changes, or "drive-by" improvements with bug fixes. When in doubt, propose the narrow fix and flag the broader concern separately.
4. **Cross-platform applies.** Linux, macOS, and Windows (ConPTY) are all tier-1. Do not assume POSIX-only APIs, `python3` launcher, `fork`-based concurrency, or Unix path conventions. CI exercises all three.
5. **No nonsense abstractions.** Do not propose traits for one implementation, builder patterns for two-field structs, or generic helpers used by one caller. Three similar lines is better than a premature abstraction. The classifier and matcher layers were intentionally flattened; do not re-layer them.
6. **TDD for behavior changes.** A new fixture under `tests/fixtures/claude_code/<name>.txt` + sibling `.expected.json` is the conventional way to lock a classifier change. A new test in `tests/scripts/test_claude_stream.py` (stdlib `unittest`, NO pytest) locks Python script behavior.

## Common review anti-patterns to avoid

- **"Also accept ASCII `>` as a prompt"** — looks reasonable in isolation, breaks Markdown blockquotes in Claude's answer prose. The rule: `❯ <text>` is prompt-with-text, bare `>` (alone or with only whitespace) is the idle-prompt fallback, `> <text>` is Markdown content and must NOT be classified as a prompt row.
- **"Tighten regex by requiring exact match at line end"** — Claude TUI spinners render `✻ Thinking…` AND `✻ Thinking… (12s · ↓ N tokens · thinking)`. The trailing parenthesized counter is the most common shape on Sonnet 4.6 extended thinking and Explore-subagent runs. Patterns that require `…` at literal line end miss this shape and the classifier stays "no completion marker on screen" forever.
- **"Move spinner detection to body-only"** — during Explore-subagent runs the body is frozen and the spinner only repaints in the status bar. Use full-screen scans (`screen_text`, not `body_text`) for active-work indicators. Status-bar rows have a known structural shape (`user @ host  [Model X.Y]`, `⏵⏵ auto mode on …`) and don't match any of the spinner/tool-progress patterns.
- **"Add a bytes-grown threshold to confirm completion"** — these are timing heuristics. The transcript can be small for legitimate completions (Haiku ack-and-stop) and large for premature exits (subagent intermediate frames). Substance is structural, not numerical.
- **"Cache the previous response so we can dedupe"** — the classifier is stateless by design; plugin state across `M.classify` calls would require a Rust host change AND a plugin schema change. Don't propose plugin-side caching without acknowledging the host work.
- **"Add a Markdown blockquote escape for `>`"** — restrict prompt-row recognition to `❯` instead. ASCII `> <text>` is unambiguously Markdown; bare `>` is the ASCII idle fallback. The two shapes are structurally distinct.

## Conventions

- **Commits:** Conventional Commits (`feat(scope):`, `fix(scope):`, etc.). Match commit type to branch prefix.
- **Diffs:** Small and behavior-preserving unless the PR explicitly changes behavior. Preserve or improve test coverage when moving code.
- **Comments:** Default to none. Only add a comment when WHY is non-obvious (hidden constraint, subtle invariant, workaround for a specific bug, behavior that would surprise a reader). Don't reference PRs, issue numbers, or "added for X" — that belongs in the commit message.
- **Error handling:** Do not add fallbacks for scenarios that can't happen. Trust internal code and framework guarantees. Validate only at system boundaries (user input, external APIs).
- **Changelog:** User-visible changes (CLI flags, RPC methods, config keys, runtime layout, classifier states) get an entry under `[Unreleased]` in `CHANGELOG.md` in the same PR.

## Where things live

- `src/` — generic abstractions: `Target`, `Session`, `Screen`, `Action`, `Matcher`, `Transcript`, `Extension`, RPC. No Claude-specific code.
- `plugins/claude-code/` — Lua plugin (`main.lua`, `helpers.lua`, `manifest.toml`). All TUI knowledge.
- `scripts/claude-stream.py` — single-turn Python wrapper. Stdlib-only. Demonstrates every primitive a real consumer would touch.
- `tests/fixtures/claude_code/` — sanitized recorded screens + expected classification. Adding a fixture is a documentation-only change.
- `tests/scripts/test_claude_stream.py` — Python script unit tests. Stdlib `unittest`.
- `website/` — VitePress docs site.
