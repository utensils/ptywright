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
    ("session.list()", "list known adapters"),
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
    ("screen.snapshot()", "rerender the live preview"),
    ("inspect()", "diagnostic dump"),
    (":tabs", "list adapters"),
    (":focus", "switch focus to an adapter id"),
    (":notifications on", "subscribe to session.* notifications"),
    (":notifications off", "unsubscribe"),
    (":rpc ", "raw JSON-RPC escape hatch"),
    (":help", "show help"),
    (":quit", "exit the REPL"),
];

/// Conventional key names plugins generally honor in `send.key("…")`.
/// Plugins are free to handle anything (so `"\x1b"`, etc., are valid), but
/// these are the ones worth surfacing as suggestions.
const KEY_NAMES: &[(&str, &str)] = &[
    ("enter", "↩ submit"),
    ("escape", "⎋ cancel"),
    ("tab", "tab"),
    ("backspace", "⌫"),
    ("space", "space"),
    ("up", "↑"),
    ("down", "↓"),
    ("left", "←"),
    ("right", "→"),
    ("home", "home"),
    ("end", "end"),
    ("pageup", "page up"),
    ("pagedown", "page down"),
    ("delete", "delete"),
    ("insert", "insert"),
    ("y", "yes"),
    ("n", "no"),
    ("1", "first numeric option"),
    ("2", "second numeric option"),
    ("ctrl-c", "interrupt"),
    ("ctrl-d", "EOF"),
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

/// Completer for the REPL. Reads adapter ids out of [`ReplCtx`] and plugin
/// names out of a `PluginCache`, both behind locks so the TUI can update
/// them between completion requests.
pub struct ReplCompleter {
    ctx: Arc<Mutex<ReplCtx>>,
    plugins: PluginCache,
}

impl ReplCompleter {
    pub fn new(ctx: Arc<Mutex<ReplCtx>>, plugins: PluginCache) -> Self {
        Self { ctx, plugins }
    }
}

impl Completer for ReplCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let prefix_end = pos.min(line.len());
        let prefix = &line[..prefix_end];
        let token = current_token(prefix);
        let start = prefix_end - token.len();

        // `:focus <TAB>` → list adapter ids.
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
        let mut completer = ReplCompleter::new(ctx, PluginCache::new());
        let suggestions = completer.complete("", 0);
        assert!(suggestions.len() >= DSL_COMMANDS.len());
    }

    #[test]
    fn prefix_filters_dsl_table() {
        let ctx = ctx_with_adapters(&[]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new());
        let suggestions = completer.complete("ses", 3);
        assert!(suggestions.iter().all(|s| s.value.starts_with("session.")));
        assert!(!suggestions.is_empty());
    }

    #[test]
    fn focus_completes_known_adapter_ids() {
        let ctx = ctx_with_adapters(&[("e1", "claude-code"), ("e2", "claude-code")]);
        let mut completer = ReplCompleter::new(ctx, PluginCache::new());
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
        let mut completer = ReplCompleter::new(ctx, plugins);
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
        let mut completer = ReplCompleter::new(ctx, plugins);
        let line = r#"session.spawn("cla"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["claude-code"]);
    }

    #[test]
    fn current_token_stops_at_whitespace_and_paren() {
        assert_eq!(current_token("session.s"), "session.s");
        assert_eq!(current_token("foo(bar"), "bar");
        assert_eq!(current_token("foo( "), "");
    }
}
