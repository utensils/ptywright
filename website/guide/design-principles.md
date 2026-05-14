# Design principles

ptywright should be easy to extend without turning the core into a pile of app-specific shortcuts.

## Keep the core generic

Use terminal automation names in the core: target, session, screen, action, matcher, transcript, turn, adapter. Avoid naming core APIs after a single app, prompt style, or workflow.

If behavior is specific to a shell, REPL, editor, agent, or full-screen TUI, put it in an adapter.

## Prefer explicit state over sleeps

Terminal automation is flaky when it relies on fixed delays. Prefer APIs that can express:

- what output or screen state is expected;
- how long to wait;
- what counts as an error;
- what transcript or screen snapshot should be returned.

## Keep platform seams visible

PTY behavior differs across Unix and Windows. The library should hide routine differences for callers, but platform-specific requirements should stay explicit in modules, feature gates, docs, or error messages.

## Make transcripts first-class

A PTY driver is easiest to debug when every action can be explained. Future APIs should make it natural to capture:

- input actions;
- raw output where useful;
- parsed screen snapshots;
- matcher decisions;
- process exit status and timing.

## Keep the CLI boring

The binary should remain predictable and scriptable:

- stable flags;
- clear exit codes;
- machine-readable output when commands need it;
- no hidden background services unless a command explicitly starts one.
