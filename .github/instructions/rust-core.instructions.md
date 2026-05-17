---
applyTo: "src/**/*.rs,tests/**/*.rs"
---

# Rust core — review constraints

The Rust layer is **strictly generic**. It owns PTY lifecycle, the parser-backed `Screen`, the `Action`/`Matcher` traits, transcript capture, the `Extension` trait, the JSON-RPC server, and the `~/.ptywright/` runtime layout. It does NOT own Claude Code semantics, intent names, classifier state names, spinner glyphs, or any other TUI-version assumption.

## Hard rules

1. **No application-specific identifiers anywhere in `src/`** except as values inside the single `BUILTIN_PLUGINS` slice in `src/plugin.rs`. Do not propose adding a `claude_code_*` module, a typed `ClaudeState` enum, a Claude-specific RPC method, or a per-plugin matcher kind. Application semantics flow through the generic `adapter.*` JSON-RPC surface as strings.
2. **Cross-platform.** Linux, macOS, and Windows (ConPTY) are tier-1. Gate Unix-only APIs behind `#[cfg(unix)]` and provide a Windows path. CI exercises all three OSes.
3. **Locking discipline.** Per-adapter state lives behind `Arc<Mutex<ExtensionEntry>>`. The sibling `adapter_plugin: HashMap<String, String>` mirror exists specifically so `plugin.unload`'s "live adapters" check cannot race with a long-running `adapter.wait` on a contended mutex. Do not propose merging these two structures without addressing the deadlock surface.
4. **Permission gating.** Per-method permission checks happen at dispatch time in `src/rpc.rs` against the bound manifest. Returned errors are `-32004 PermissionDenied` with structured `data: { method, required_permission }`. Don't propose adding a permission check ad-hoc inside a handler — they belong in the dispatch shim.
5. **Logging mode helpers.** `serve --stdio` must NEVER write logs to stdout (stdout is JSON-RPC framing). Use `init_for_run` / `init_for_serve_stdio` / `init_for_serve_socket` / `init_for_oneshot` rather than calling `tracing_subscriber::fmt()` directly. If you propose a new entrypoint, add a matching helper.
6. **Test geometry.** Tests that exercise classifier behavior should set terminal size explicitly. The classifier-stable default for Claude Code is 60×200; smaller geometries may cause line wrapping that breaks classification. If a test relies on a different size, document why.

## Existing patterns to keep

- `Action::Key` snake_case serde — every variant has a serde-derived snake_case alias. The `key_intent_covers_every_rust_key_variant` test in `tests/lua_plugin_intents.rs` enumerates them all. New variants need a fixture-or-test that proves the alias routes correctly.
- `apply_plan` interprets `Some("")` as an explicit-clear sentinel (drop the recorded intent). `apply_plan_with_required_intent` mirrors this semantics. Both paths must keep the same interpretation.
- The `_test-fixtures` feature gates the test-only `ptywright-echo-tui` binary. Without it, `cargo test --locked` fails to compile `tests/cli_tests.rs` by design — that's the intentional contract that keeps the fixture out of `cargo install` deployments.

## Review anti-patterns

- **"Replace `Arc<Mutex<T>>` with `Arc<RwLock<T>>` for better concurrency"** — the per-adapter mutex is held during PTY I/O. Read-write split doesn't help and complicates the borrow.
- **"Add a `validate_*` helper at the start of every handler"** — validation belongs at the dispatch layer (param parsing, permission check). Internal handlers trust their inputs.
- **"Use anyhow for error handling"** — the crate uses a typed `Error`/`Result` for library callers. Don't propose mixing anyhow into the public surface.
- **"Add `#[derive(Clone)]` so callers don't have to clone manually"** — most internal types deliberately don't implement Clone; if a caller needs Clone, they Arc-wrap.

## When updating handlers

- Mirror permission checks for new methods in the dispatch match arm.
- Update the gating test (`tests/cli_tests.rs` end-to-end JSON-RPC) if you add a new method.
- Document the new method in `website/reference/json-rpc.md` in the same PR.
