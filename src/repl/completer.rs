//! reedline `Completer` impl for the REPL's DSL.
//!
//! The completion set is small enough to hand-curate: a static table of
//! command paths plus context-aware dynamic entries (live adapter ids for
//! `:focus`, cached plugin names for `session.spawn`).

use std::sync::{Arc, Mutex};

use reedline::{Completer, Span, Suggestion};

use super::ctx::ReplCtx;

/// Snippets the completer emits unconditionally — every DSL form that has
/// a fixed prefix is listed here so tab completion always surfaces them
/// even in an empty buffer.
const DSL_COMMANDS: &[(&str, &str)] = &[
    ("plugins()", "list built-in plugins"),
    ("session.spawn(\"\")", "spawn an adapter for a plugin"),
    ("session.list()", "list adapters (this REPL)"),
    ("session.live()", "list adapters live on the server"),
    (
        "session.attach(\"\")",
        "attach a server adapter into this REPL",
    ),
    ("session.attach(\"all\")", "attach every live adapter"),
    ("session.close()", "close the focused adapter"),
    ("state()", "re-classify the focused adapter"),
    ("send.text(\"\")", "send a prompt to the focused adapter"),
    ("send.key(\"\")", "send a single key"),
    ("send.intent(\"\")", "invoke a plugin intent by name"),
    (
        "wait(matches(r\"\"))",
        "wait until the screen matches a regex",
    ),
    (
        "wait(screen_stable(250ms))",
        "wait for the screen to settle",
    ),
    (
        "transcript.snapshot()",
        "dump the focused adapter's transcript",
    ),
    (
        "screen.snapshot()",
        "render the focused PTY inline (styled)",
    ),
    ("view()", "alias for screen.snapshot()"),
    ("inspect()", "diagnostic dump"),
    (":tabs", "list adapters"),
    (":focus", "switch focus to an adapter id"),
    (":live", "list adapters live on the server"),
    (":attach", "attach a server adapter (id or `all`)"),
    (":notifications on", "subscribe to session.* notifications"),
    (":notifications off", "unsubscribe"),
    (":rpc ", "raw JSON-RPC escape hatch"),
    (":help", "show help"),
    (":quit", "exit the REPL"),
];

/// Names the host's `action.key(...)` recognises plus the most common
/// single-char text tokens that the plugin's generic `key` intent forwards
/// through `action.text`. Ordering matters — the completer surfaces these
/// in declaration order, so the most-asked-for keys live at the top and
/// the long tail (less-used ctrl combos, F-keys, single-char fallthrough)
/// trails behind. Suggestions outside this list either need a host enum
/// variant added in `src/action.rs::Key` or are intentionally sent as
/// literal text by the plugin's `M.key` fallthrough.
const KEY_NAMES: &[(&str, &str)] = &[
    // ── Submission / line editing ───────────────────────────
    ("enter", "↩ submit"),
    ("escape", "⎋ cancel"),
    ("tab", "↹"),
    ("shift-tab", "⇧↹ back-tab"),
    ("backspace", "⌫"),
    ("delete", "⌦"),
    ("space", "␣"),
    // ── Arrows ──────────────────────────────────────────────
    ("up", "↑"),
    ("down", "↓"),
    ("left", "←"),
    ("right", "→"),
    // ── Navigation cluster ──────────────────────────────────
    ("home", "⤒ line / page start"),
    ("end", "⤓ line / page end"),
    ("page-up", "PgUp"),
    ("page-down", "PgDn"),
    ("insert", "Ins"),
    // ── Common ctrl combos (readline / emacs defaults) ──────
    ("ctrl-c", "interrupt"),
    ("ctrl-d", "EOF"),
    ("ctrl-l", "clear / refresh"),
    ("ctrl-r", "reverse search"),
    ("ctrl-u", "kill to line start"),
    ("ctrl-w", "kill previous word"),
    ("ctrl-a", "line start"),
    ("ctrl-e", "line end"),
    ("ctrl-k", "kill to line end"),
    ("ctrl-y", "yank"),
    ("ctrl-z", "suspend"),
    // ── Less-common ctrl combos (still routed through Key) ──
    ("ctrl-b", "back one char"),
    ("ctrl-f", "forward one char"),
    ("ctrl-g", "bell / cancel"),
    ("ctrl-n", "next line / history forward"),
    ("ctrl-o", "newline-and-yank"),
    ("ctrl-p", "previous line / history back"),
    ("ctrl-q", "quoted-insert / XON"),
    ("ctrl-s", "forward search / XOFF"),
    ("ctrl-t", "transpose chars"),
    ("ctrl-v", "verbatim-insert"),
    ("ctrl-x", "chord prefix"),
    // ── Function keys ───────────────────────────────────────
    ("f1", ""),
    ("f2", ""),
    ("f3", ""),
    ("f4", ""),
    ("f5", ""),
    ("f6", ""),
    ("f7", ""),
    ("f8", ""),
    ("f9", ""),
    ("f10", ""),
    ("f11", ""),
    ("f12", ""),
    // ── Text fallthrough (kept for muscle memory) ───────────
    ("y", "yes (sent as text)"),
    ("n", "no (sent as text)"),
    ("1", "first numeric option (sent as text)"),
    ("2", "second numeric option (sent as text)"),
];

/// Cached plugin names from the most recent `adapter.list` response.
/// `Arc<Mutex<_>>` so the TUI thread can refresh it after a `plugins()`
/// roundtrip without blocking the completer's `&mut` call site.
#[derive(Debug, Default, Clone)]
pub struct PluginCache {
    inner: Arc<Mutex<Vec<String>>>,
}

impl PluginCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, plugins: Vec<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = plugins;
        }
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Cached server-side live adapter ids. The TUI seeds this from the
/// initial `adapter.live` probe; the completer suggests these inside
/// `:attach <TAB>` so the operator can complete ids that no other
/// connection has loaded locally.
#[derive(Debug, Default, Clone)]
pub struct AdapterCache {
    inner: Arc<Mutex<Vec<String>>>,
}

impl AdapterCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, ids: Vec<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = ids;
        }
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Completer for the REPL. Reads adapter ids out of [`ReplCtx`] and plugin
/// names out of a `PluginCache`, both behind locks so the TUI can update
/// them between completion requests.
pub struct ReplCompleter {
    ctx: Arc<Mutex<ReplCtx>>,
    plugins: PluginCache,
    adapters: AdapterCache,
}

impl ReplCompleter {
    pub fn new(ctx: Arc<Mutex<ReplCtx>>, plugins: PluginCache, adapters: AdapterCache) -> Self {
        Self {
            ctx,
            plugins,
            adapters,
        }
    }
}

impl Completer for ReplCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let prefix_end = pos.min(line.len());
        let prefix = &line[..prefix_end];
        let token = current_token(prefix);
        let start = prefix_end - token.len();

        // `:focus <TAB>` → list local adapter ids.
        if prefix.trim_start().starts_with(":focus") {
            let after = prefix.trim_start_matches(":focus").trim_start();
            let after_len = after.len();
            let after_start = prefix_end - after_len;
            let span = Span::new(after_start, prefix_end);
            let ctx = self.ctx.lock().ok();
            let ids: Vec<String> = match ctx {
                Some(ctx) => ctx.adapters.iter().map(|tab| tab.id.clone()).collect(),
                None => Vec::new(),
            };
            return ids
                .into_iter()
                .filter(|id| id.starts_with(after))
                .map(|id| Suggestion {
                    value: id,
                    description: Some("adapter id".into()),
                    span,
                    append_whitespace: false,
                    ..Default::default()
                })
                .collect();
        }

        // `:attach <TAB>` → `all`, plus the union of locally-known adapter
        // ids and the server-side live ids cached at startup.
        if prefix.trim_start().starts_with(":attach") {
            let after = prefix.trim_start_matches(":attach").trim_start();
            let after_len = after.len();
            let after_start = prefix_end - after_len;
            let span = Span::new(after_start, prefix_end);
            let mut suggestions: Vec<Suggestion> = Vec::new();
            let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            if "all".starts_with(after) {
                suggestions.push(Suggestion {
                    value: "all".to_string(),
                    description: Some("attach every live adapter".into()),
                    span,
                    append_whitespace: false,
                    ..Default::default()
                });
            }
            for id in self.adapters.snapshot() {
                if id.starts_with(after) && seen.insert(id.clone()) {
                    suggestions.push(Suggestion {
                        value: id,
                        description: Some("server-side adapter".into()),
                        span,
                        append_whitespace: false,
                        ..Default::default()
                    });
                }
            }
            let ctx = self.ctx.lock().ok();
            if let Some(ctx) = ctx {
                for tab in &ctx.adapters {
                    if tab.id.starts_with(after) && seen.insert(tab.id.clone()) {
                        suggestions.push(Suggestion {
                            value: tab.id.clone(),
                            description: Some("adapter id".into()),
                            span,
                            append_whitespace: false,
                            ..Default::default()
                        });
                    }
                }
            }
            return suggestions;
        }

        // `session.spawn("<TAB>` → plugin names.
        if let Some(stem) = prefix.rfind("session.spawn(\"") {
            let inside_start = stem + "session.spawn(\"".len();
            if inside_start <= prefix_end {
                let inside = &prefix[inside_start..prefix_end];
                let span = Span::new(inside_start, prefix_end);
                return self
                    .plugins
                    .snapshot()
                    .into_iter()
                    .filter(|name| name.starts_with(inside))
                    .map(|name| Suggestion {
                        value: name,
                        description: Some("plugin".into()),
                        span,
                        append_whitespace: false,
                        ..Default::default()
                    })
                    .collect();
            }
        }

        // `send.key("<TAB>` → conventional key names. Plugins ultimately
        // decide how to interpret the string, but every TUI handles the
        // names below — surface them to save the user from typing them out.
        if let Some(stem) = prefix.rfind("send.key(\"") {
            let inside_start = stem + "send.key(\"".len();
            if inside_start <= prefix_end {
                let inside = &prefix[inside_start..prefix_end];
                let span = Span::new(inside_start, prefix_end);
                return KEY_NAMES
                    .iter()
                    .filter(|(name, _)| name.starts_with(inside))
                    .map(|(name, hint)| Suggestion {
                        value: (*name).to_string(),
                        description: Some((*hint).to_string()),
                        span,
                        append_whitespace: false,
                        ..Default::default()
                    })
                    .collect();
            }
        }

        // Default: filter the static command table by current-token prefix.
        let span = Span::new(start, prefix_end);
        DSL_COMMANDS
            .iter()
            .filter(|(value, _)| value.starts_with(token))
            .map(|(value, description)| Suggestion {
                value: (*value).to_string(),
                description: Some((*description).to_string()),
                span,
                append_whitespace: false,
                ..Default::default()
            })
            .collect()
    }
}

/// Return the "current token" being typed at the end of `prefix` — the
/// trailing run of identifier-ish characters used for filtering the
/// static command table. Whitespace, parentheses, quotes, etc. terminate
/// the token.
fn current_token(prefix: &str) -> &str {
    let bytes = prefix.as_bytes();
    let mut start = bytes.len();
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == ':' {
            start -= 1;
        } else {
            break;
        }
    }
    &prefix[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_adapters(ids: &[(&str, &str)]) -> Arc<Mutex<ReplCtx>> {
        let mut ctx = ReplCtx::new();
        for (id, plugin) in ids {
            ctx.upsert_adapter(id, plugin);
        }
        Arc::new(Mutex::new(ctx))
    }

    #[test]
    fn empty_buffer_returns_full_dsl_table() {
        let ctx = ctx_with_adapters(&[]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new(), AdapterCache::new());
        let suggestions = completer.complete("", 0);
        assert!(suggestions.len() >= DSL_COMMANDS.len());
    }

    #[test]
    fn prefix_filters_dsl_table() {
        let ctx = ctx_with_adapters(&[]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new(), AdapterCache::new());
        let suggestions = completer.complete("ses", 3);
        assert!(suggestions.iter().all(|s| s.value.starts_with("session.")));
        assert!(!suggestions.is_empty());
    }

    #[test]
    fn focus_completes_known_adapter_ids() {
        let ctx = ctx_with_adapters(&[("e1", "claude-code"), ("e2", "claude-code")]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new(), AdapterCache::new());
        let suggestions = completer.complete(":focus ", 7);
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(values.contains(&"e1"));
        assert!(values.contains(&"e2"));
    }

    #[test]
    fn spawn_completes_cached_plugin_names() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into(), "future-plugin".into()]);
        let mut completer = ReplCompleter::new(ctx, plugins, AdapterCache::new());
        let line = r#"session.spawn(""#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(values.contains(&"claude-code"));
        assert!(values.contains(&"future-plugin"));
    }

    #[test]
    fn spawn_completion_respects_partial_prefix() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into(), "rspec".into()]);
        let mut completer = ReplCompleter::new(ctx, plugins, AdapterCache::new());
        let line = r#"session.spawn("cla"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["claude-code"]);
    }

    #[test]
    fn attach_completer_suggests_all_and_known_ids() {
        let ctx = ctx_with_adapters(&[("e7", "claude-code")]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new(), AdapterCache::new());
        let line = ":attach ";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(values.contains(&"all"), "expected `all`, got {values:?}");
        assert!(values.contains(&"e7"), "expected `e7`, got {values:?}");
    }

    #[test]
    fn attach_completer_surfaces_server_only_ids() {
        // No local tab for `e9` — only the server-side adapter cache
        // knows it. The completer must still surface it inside `:attach`.
        let ctx = ctx_with_adapters(&[]);
        let server = AdapterCache::new();
        server.set(vec!["e9".into(), "e10".into()]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new(), server);
        let line = ":attach e";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(values.contains(&"e9"), "expected `e9`, got {values:?}");
        assert!(values.contains(&"e10"), "expected `e10`, got {values:?}");
    }

    #[test]
    fn attach_completer_dedupes_local_and_server_overlap() {
        let ctx = ctx_with_adapters(&[("e1", "claude-code")]);
        let server = AdapterCache::new();
        server.set(vec!["e1".into(), "e2".into()]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new(), server);
        let suggestions = completer.complete(":attach e", ":attach e".len());
        let count_e1 = suggestions.iter().filter(|s| s.value == "e1").count();
        assert_eq!(count_e1, 1, "expected one `e1` suggestion, got {count_e1}");
    }

    #[test]
    fn current_token_stops_at_whitespace_and_paren() {
        assert_eq!(current_token("session.s"), "session.s");
        assert_eq!(current_token("foo(bar"), "bar");
        assert_eq!(current_token("foo( "), "");
    }

    #[test]
    fn send_key_completer_lists_submission_keys_first() {
        // Pin the "most-common-first" contract for the key completion
        // list: enter / escape / tab / shift-tab / backspace must all
        // appear before any function key or text-fallthrough entry, so
        // tab-completing in `send.key("` surfaces them at the top of
        // the picker.
        let ctx = ctx_with_adapters(&[]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new(), AdapterCache::new());
        let line = r#"send.key(""#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();

        let pos = |needle: &str| {
            values
                .iter()
                .position(|v| *v == needle)
                .unwrap_or(usize::MAX)
        };
        let enter = pos("enter");
        let escape = pos("escape");
        let shift_tab = pos("shift-tab");
        let f1 = pos("f1");
        let yes = pos("y");

        assert!(enter < shift_tab, "expected enter before shift-tab");
        assert!(escape < shift_tab, "expected escape before shift-tab");
        assert!(shift_tab < f1, "expected shift-tab before f1");
        assert!(f1 < yes, "expected f1 before text-fallthrough y");
    }

    #[test]
    fn send_key_completer_filters_by_partial_prefix() {
        // Typing `shi` inside `send.key("` should narrow to the
        // `shift-tab` suggestion — proves the prefix filter still
        // works against the expanded table.
        let ctx = ctx_with_adapters(&[]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new(), AdapterCache::new());
        let line = r#"send.key("shi"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["shift-tab"]);
    }
}
