# P9 Spike — Subagent / background-task intents for claude-code

## Status

**Investigation-pending.** Implementation blocked on hands-on time with a recent Claude Code build (≥ 2.1.x) to characterise the actual TUI surface for background-task control. This document captures what's known, what needs to be discovered, and the decision points that gate implementation. Pick it up when you have time to drive a real `claude` session interactively.

## Why this exists

Claudette's persistent chat session emits a `task_stop` message to interrupt named background subagents Claude spawns through the Task tool — see `src/agent/harness.rs::send_task_stop` in [`claudette`](https://github.com/utensils/claudette). The current `claude-code` ptywright plugin has no intent to drive that flow, so consumers who'd otherwise drop `claude -p` for a PTY-driven session lose subagent control as a side-effect.

P9 in [the ptywright Claude Code gap plan](../../plans/study-and-understand-the-witty-leaf.md) covers adding plugin intents that drive the same flow through the TUI. The plan flagged it as a one-day spike before sizing because the TUI surface for background tasks is not documented in Claude Code's user-facing help.

## What we don't know yet

These are the questions the spike needs to answer before code goes in. Each maps to a specific thing you'd test in a live Claude Code session:

1. **How does Claude Code surface live background tasks in the TUI?**
   - A dedicated `/tasks` slash command? Open with `Ctrl-T`? A status-line indicator the user has to scroll into? Something else?
   - Capture a screenshot or fixture text of the rendered task panel.

2. **How does the operator pick a task to act on?**
   - Numbered list (`1. Foo, 2. Bar`) like the trust dialog? Cursor selection with arrow keys? Free-text task name?
   - Determines whether the plugin needs a `task.list` intent that returns parsed entries vs. a single `task.stop_active` intent.

3. **What does "stop" map to in the TUI?**
   - Ctrl-C while a task panel is focused? A confirmation dialog like permission gates?
   - This determines whether the plugin can express stop atomically or needs to chain intents.

4. **Is there visible state for tasks that completed vs. running vs. errored?**
   - Drives whether `task.list` should return a structured `{ id, name, state }` shape or just a flat list of names.

5. **Does background task state ever reach the classifier as a top-level state worth detecting?**
   - E.g. "task X failed and is blocking the prompt." If so, we'd add a new classifier state (currently 9; would become 10).
   - Or it stays in the metadata channel established by P5 and doesn't earn a top-level state.

## Recommended workflow for the spike

1. Start a Claude Code session and ask it to spawn a few background tasks via the Task tool (e.g. "spawn three parallel tasks that each sleep 30 seconds and report when done"). This is the easiest way to populate live task state.
2. Open the TUI's task-control surface, capture the screen at each interesting moment to `tests/fixtures/claude_code/task_*.txt`. Aim for:
   - `task_list.txt` — multiple tasks live
   - `task_stop_dialog.txt` — mid-stop confirmation if one exists
   - `task_stopped.txt` — post-stop steady state
3. Pin the answers above into this doc.
4. Decide whether the plugin needs new classifier states or only new intents.

## Likely implementation shape (subject to spike findings)

Assuming the TUI exposes a task panel reachable by some key or slash command, expect to add roughly:

- `plugins/claude-code/main.lua`:
  - New intent `task.list` returning structured metadata (relies on P5's metadata channel) — or, if the TUI requires a key sequence to open the panel, an intent `task.open_panel` plus reading via subsequent `adapter.snapshot` / `adapter.state`.
  - New intent `task.stop` taking either a task id/name or an "active" marker.
  - Possibly new classifier state `viewing_tasks` if the panel is its own modal screen.
- `tests/fixtures/claude_code/task_*.txt` + `.expected.json` — pinning the parsed metadata shape.
- `tests/lua_plugin_intents.rs` — intent contract tests for the action plans.
- No changes expected in `src/` since the metadata channel and snapshot-based classifier are already in place.

## Out of scope

- Programmatic visibility into the Task tool's individual subagent transcripts — that lives in the JSONL session files Claude Code writes, not in the rendered TUI. Consumers can tail those files independently of ptywright (same architecture the parent plan recommends for assistant output).
- Background-task scheduling or supervision. ptywright drives whatever surface the TUI already exposes; it does not invent its own task model.

## Sizing once the spike lands

Once questions 1–5 above are answered:

- **Small** (S, ~half-day): if the TUI surfaces tasks via existing primitives the plugin already knows (modal dialog, numbered options) — looks like the permission-dialog parser (P10) writ slightly larger.
- **Medium** (M, ~1–2 days): if a new classifier state is required, or if "stop" chains multiple intents through a confirmation dialog.
- **Large** (L, week): if the TUI uses an interactive selection model (arrow-key cursor on a list, modal focus switches) that doesn't map to existing intent shapes — would need new action primitives in the host.

Probably S or M.
