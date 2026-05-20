//! reedline `Completer` impl backed by Lua-VM introspection.
//!
//! Instead of maintaining a static `DSL_COMMANDS` table that drifts the
//! moment a binding is added, the completer iterates the live Lua VM:
//!
//! 1. **Top-level identifier** (`sess<TAB>` → `session`): iterate
//!    [`mlua::Lua::globals`]'s `pairs()`, intersect with the
//!    REPL-globals whitelist provided by [`super::lua::LuaRepl::repl_globals`].
//!    The whitelist filters out `string` / `math` / `io` / `_G` / `_VERSION`
//!    so they don't drown the picker.
//! 2. **Member access** (`session.<TAB>` → `spawn` / `resume` / …): look
//!    up the head identifier in globals, iterate `pairs()` on the
//!    resulting table.
//! 3. **String-arg completion** (`session.spawn("<TAB>`, `send.key("<TAB>`,
//!    `:attach <TAB>`): unchanged from the legacy DSL — `PluginCache`,
//!    `AdapterCache`, and the static [`KEY_NAMES`] table. These paths
//!    never enter the Lua VM and keep the completer responsive even
//!    while a slow `eval` is running on the same mutex.
//!
//! The Lua mutex is shared with `LuaRepl::eval` and `LuaValidator`, but
//! reedline serialises completer / validator / eval on the input thread,
//! so contention is theoretical.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

use mlua::{Lua, Table, Value};
use reedline::{Completer, Span, Suggestion};

use super::ctx::ReplCtx;

/// Names the host's `action.key(...)` recognises plus the most common
/// single-char text tokens that the plugin's generic `key` intent
/// forwards through `action.text`. Ordering matters — the completer
/// surfaces these in declaration order, so the most-asked-for keys live
/// at the top of the picker and the long tail trails behind.
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
    // ── Common ctrl combos ──────────────────────────────────
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
    // ── Less-common ctrl combos ─────────────────────────────
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

/// Inline help strings for each REPL-bound global and the named members
/// of the callable / namespace tables. The lookup key is the
/// dotted-qualified path (top-level globals use the bare name); a missing
/// entry leaves the picker's description column blank, which is fine for
/// any new binding that ships later.
///
/// Keeping this in the completer (not in `lua.rs`) means the help text
/// stays local to the surface that uses it; the bindings themselves
/// don't get muddied with reedline-flavoured strings.
const DESCRIPTIONS: &[(&str, &str)] = &[
    // Top-level globals
    ("plugins", "list / describe built-in plugins"),
    (
        "plugins.describe",
        "describe a plugin's intents / matchers / states",
    ),
    ("session", "spawn / list / attach / close adapters"),
    ("session.spawn", "spawn an adapter for a plugin"),
    ("session.resume", "resume + close prior adapter"),
    ("session.list", "list local adapter tabs (this REPL)"),
    ("session.live", "list adapters live on the server"),
    ("session.attach", "adopt a server-side adapter (or `all`)"),
    ("session.close", "close the focused (or named) adapter"),
    ("send", "drive the focused adapter (text / key / intent)"),
    ("send.text", "send a prompt (intent=send_prompt)"),
    ("send.key", "send a single named key (enter, shift-tab, …)"),
    ("send.intent", "invoke an arbitrary plugin intent by name"),
    (
        "wait",
        "wait for a matcher · also wait.matches / wait.screen_stable",
    ),
    ("wait.matches", "shorthand: wait for a regex match"),
    (
        "wait.screen_stable",
        "shorthand: wait for screen stability (ms)",
    ),
    ("matches", "regex matcher constructor (tagged table)"),
    (
        "screen_stable",
        "screen-stability matcher constructor (tagged table)",
    ),
    ("cancel_wait", "break a still-in-flight wait by wait_id"),
    ("turn", "atomic adapter.turn (send + wait_turn_matcher)"),
    ("state", "re-classify the focused adapter"),
    ("transcript", "transcript queries · .snapshot()"),
    (
        "transcript.snapshot",
        "dump the focused adapter's transcript",
    ),
    ("screen", "screen queries · .snapshot()"),
    ("screen.snapshot", "render the focused PTY inline (styled)"),
    ("view", "alias for screen.snapshot()"),
    ("inspect", "diagnostic adapter.inspect dump"),
    ("re", "identity helper · re('…') documents regex intent"),
    ("ms", "identity helper · ms(N) documents milliseconds"),
    ("s", "seconds → milliseconds · s(2) = 2000"),
];

fn description_for(qualified_name: &str) -> Option<String> {
    DESCRIPTIONS
        .iter()
        .find(|(k, _)| *k == qualified_name)
        .map(|(_, v)| (*v).to_string())
}

/// Identifiers that are *always* called with no arguments. When the
/// completer picks one of these, it appends `()` to the suggestion
/// value so the operator doesn't have to retype the call form (matches
/// the rotating tip's "did you mean to call it with `()`?" hint and
/// removes the dead-end interaction where Tab on `view` lands on a
/// bare function reference).
///
/// Deliberately excluded:
///   * `transcript.snapshot` — accepts a `{ redact = bool }` table; the
///     paren form would force a backspace to switch to `{ }`.
///   * `session.spawn` / `send.text` / `wait` / … — every other
///     callable takes arguments; the existing string-arg completion
///     handles `<TAB>` inside `f(`/`f{` instead.
const NULLARY_GLOBALS: &[&str] = &[
    "view",
    "state",
    "inspect",
    "plugins",
    "session.list",
    "session.live",
    "screen.snapshot",
];

fn is_nullary(qualified_name: &str) -> bool {
    NULLARY_GLOBALS.contains(&qualified_name)
}

/// Static meta-command suggestions. Unlike the DSL, meta is parsed in
/// `meta.rs` so the completer can't introspect it — keeping a small table
/// here is the cheapest path.
const META_COMMANDS: &[(&str, &str)] = &[
    (":tabs", "list adapters"),
    (":focus", "switch focus to an adapter id"),
    (":live", "list adapters live on the server"),
    (":attach", "attach a server adapter (id or `all`)"),
    (":notifications on", "subscribe to session.* notifications"),
    (
        ":notifications on adapters=",
        "subscribe; filter to named adapters",
    ),
    (
        ":notifications on sessions=",
        "subscribe; filter to named sessions",
    ),
    (":notifications off", "unsubscribe"),
    (":rpc ", "raw JSON-RPC escape hatch"),
    (":tips", "guided tour of the most useful idioms"),
    (":help", "show help"),
    (":quit", "exit the REPL"),
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

/// Completer for the REPL. Sourced from:
///
/// * the live Lua VM (top-level identifiers + member access),
/// * a `PluginCache` (`session.spawn("…<TAB>`),
/// * an `AdapterCache` (`:attach <TAB>`),
/// * the operator's local tab list ([`ReplCtx::adapters`]),
/// * the static [`KEY_NAMES`] / [`META_COMMANDS`] tables.
pub struct ReplCompleter {
    ctx: Arc<Mutex<ReplCtx>>,
    plugins: PluginCache,
    adapters: AdapterCache,
    lua: Arc<Mutex<Lua>>,
    repl_globals: Arc<HashSet<&'static str>>,
}

impl ReplCompleter {
    pub fn new(
        ctx: Arc<Mutex<ReplCtx>>,
        plugins: PluginCache,
        adapters: AdapterCache,
        lua: Arc<Mutex<Lua>>,
        repl_globals: Arc<HashSet<&'static str>>,
    ) -> Self {
        Self {
            ctx,
            plugins,
            adapters,
            lua,
            repl_globals,
        }
    }
}

impl Completer for ReplCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let prefix_end = pos.min(line.len());
        let prefix = &line[..prefix_end];

        // String-arg completions go first — they take priority over the
        // identifier-introspection paths because the operator is inside a
        // string literal, not typing a Lua identifier.
        if let Some(suggestions) = complete_string_arg(prefix, prefix_end, self) {
            return suggestions;
        }

        // `:focus <id>` / `:attach <id|all>` — adapter ids.
        if let Some(suggestions) = complete_meta_arg(prefix, prefix_end, self) {
            return suggestions;
        }

        let token = current_token(prefix);
        let start = prefix_end - token.len();
        let span = Span::new(start, prefix_end);

        // Meta command prefix `:foo<TAB>` — surface the static table.
        if token.starts_with(':') {
            return META_COMMANDS
                .iter()
                .filter(|(value, _)| value.starts_with(token))
                .map(|(value, description)| Suggestion {
                    value: (*value).to_string(),
                    description: Some((*description).to_string()),
                    span,
                    append_whitespace: false,
                    ..Default::default()
                })
                .collect();
        }

        // Identifier completion. If the token contains a `.`, complete the
        // member name against the head's table; otherwise complete the
        // top-level identifier against the REPL globals whitelist.
        let lua = match self.lua.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        if let Some((head, rest)) = token.rsplit_once('.') {
            // `session.<TAB>` — iterate the named table.
            let members = members_of(&lua, head).unwrap_or_default();
            let member_start = start + head.len() + 1; // `+1` for the `.`
            let member_span = Span::new(member_start, prefix_end);
            members
                .into_iter()
                .filter(|name| name.starts_with(rest))
                .map(|name| {
                    let qualified = format!("{head}.{name}");
                    let value = if is_nullary(&qualified) {
                        format!("{name}()")
                    } else {
                        name
                    };
                    Suggestion {
                        description: description_for(&qualified),
                        value,
                        span: member_span,
                        append_whitespace: false,
                        ..Default::default()
                    }
                })
                .collect()
        } else {
            // Top-level identifier — intersect Lua globals with our whitelist.
            top_level_completions(&lua, &self.repl_globals, token, span)
        }
    }
}

/// Inside `:focus` / `:attach`, the trailing token is an adapter id (or
/// the literal `all` for attach). The Lua VM doesn't enter into it.
fn complete_meta_arg(
    prefix: &str,
    prefix_end: usize,
    completer: &ReplCompleter,
) -> Option<Vec<Suggestion>> {
    let trimmed = prefix.trim_start();
    let leading_ws = prefix.len() - trimmed.len();
    if let Some(after) = trimmed.strip_prefix(":focus") {
        if !after.starts_with(' ') && !after.is_empty() {
            return None;
        }
        let after = after.trim_start();
        let after_start = prefix_end - after.len();
        let span = Span::new(after_start, prefix_end);
        let ctx = completer.ctx.lock().ok()?;
        let ids: Vec<String> = ctx.adapters.iter().map(|tab| tab.id.clone()).collect();
        drop(ctx);
        return Some(
            ids.into_iter()
                .filter(|id| id.starts_with(after))
                .map(|id| Suggestion {
                    value: id,
                    description: Some("adapter id".into()),
                    span,
                    append_whitespace: false,
                    ..Default::default()
                })
                .collect(),
        );
    }
    if let Some(after) = trimmed.strip_prefix(":attach") {
        if !after.starts_with(' ') && !after.is_empty() {
            return None;
        }
        let after = after.trim_start();
        let after_start = prefix_end - after.len();
        let span = Span::new(after_start, prefix_end);
        let mut suggestions: Vec<Suggestion> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        if "all".starts_with(after) {
            suggestions.push(Suggestion {
                value: "all".to_string(),
                description: Some("attach every live adapter".into()),
                span,
                append_whitespace: false,
                ..Default::default()
            });
        }
        for id in completer.adapters.snapshot() {
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
        if let Ok(ctx) = completer.ctx.lock() {
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
        return Some(suggestions);
    }
    // Silence unused-var on leading_ws for future-proofing; the meta
    // matchers above only care about the trim_start'd remainder.
    let _ = leading_ws;
    None
}

/// Kinds of first-positional string arguments the completer knows how
/// to populate. The set is intentionally small — adding a new kind means
/// pairing a [`StringArgCall`] entry with a match arm in
/// [`suggestions_for_string_arg`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringArgKind {
    /// A plugin name (`session.spawn`, `session.resume`, `plugins.describe`).
    PluginName,
    /// A named key recognised by the host (`send.key`).
    KeyName,
    /// An adapter id from the local tab list (`session.close`).
    LocalAdapterId,
    /// An adapter id plus the literal `all` (`session.attach`).
    AdapterIdOrAll,
}

/// Callable → string-arg kind. The completer scans the prefix for the
/// rightmost occurrence of any `name` here and, if the trailing text
/// looks like a first-string-argument context, surfaces the matching
/// suggestion set. Order doesn't matter — the rightmost match wins.
const STRING_ARG_CALLS: &[(&str, StringArgKind)] = &[
    ("session.spawn", StringArgKind::PluginName),
    ("session.resume", StringArgKind::PluginName),
    ("plugins.describe", StringArgKind::PluginName),
    ("send.key", StringArgKind::KeyName),
    ("session.attach", StringArgKind::AdapterIdOrAll),
    ("session.close", StringArgKind::LocalAdapterId),
];

/// Result of recognising a first-string-argument context. `inside` is
/// whatever the operator has already typed between the opening quote and
/// the cursor (empty when the quote itself hasn't been typed yet);
/// `inside_start` is its absolute byte offset within the original prefix.
struct StringArgContext<'a> {
    kind: StringArgKind,
    inside: &'a str,
    inside_start: usize,
    /// `true` when the operator typed only the opener (`(`, `{`, or
    /// nothing yet) — completions must surround the suggestion value
    /// with quotes themselves.
    needs_opening_quote: bool,
}

/// Recognise a first-string-argument context anywhere in `prefix`. The
/// rightmost matching call wins, so a chained / multi-statement line
/// resolves to the call the cursor is actually inside of.
fn detect_string_arg(prefix: &str) -> Option<StringArgContext<'_>> {
    let mut best: Option<(usize, StringArgContext<'_>)> = None;
    for (name, kind) in STRING_ARG_CALLS {
        let Some(call_at) = prefix.rfind(name) else {
            continue;
        };
        // Reject matches that are tails of a longer identifier (e.g.
        // `xsession.spawn` must not bind to `session.spawn`).
        if call_at > 0 {
            let prev = prefix.as_bytes()[call_at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        let after_start = call_at + name.len();
        let after = &prefix[after_start..];
        let Some(arg) = parse_first_arg_string(after) else {
            continue;
        };
        let context = StringArgContext {
            kind: *kind,
            inside: arg.text,
            inside_start: after_start + arg.text_offset,
            needs_opening_quote: arg.needs_opening_quote,
        };
        if best.as_ref().is_none_or(|(prev, _)| call_at > *prev) {
            best = Some((call_at, context));
        }
    }
    best.map(|(_, ctx)| ctx)
}

/// Internal parse result for the slice immediately after the call name.
struct ParsedArg<'a> {
    text: &'a str,
    text_offset: usize,
    needs_opening_quote: bool,
}

/// Parse the slice after a call name and decide whether the cursor is
/// inside the first positional string argument. Accepts three Lua
/// call shapes:
///
///   * Paren / brace form: `(`, `{` (with optional internal whitespace).
///   * String sugar: `f"x"` or `f "x"` — name immediately followed (or
///     followed after whitespace) by a quote.
///
/// The first non-whitespace character after the opener (or after the
/// name for the sugar form) must be a `"` / `'`. Anything else returns
/// `None` so kwarg-style first args (`{ rows = 24, ... }`) and
/// identifier-style positional args (`{ var, ... }`) fall through to
/// the regular Lua identifier completer.
fn parse_first_arg_string(s: &str) -> Option<ParsedArg<'_>> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut idx = 0;
    while idx < bytes.len() && (bytes[idx] == b' ' || bytes[idx] == b'\t') {
        idx += 1;
    }
    if idx == bytes.len() {
        return None;
    }
    let first = bytes[idx];
    if first == b'(' || first == b'{' {
        idx += 1;
        while idx < bytes.len() && (bytes[idx] == b' ' || bytes[idx] == b'\t') {
            idx += 1;
        }
        if idx == bytes.len() {
            return Some(ParsedArg {
                text: "",
                text_offset: idx,
                needs_opening_quote: true,
            });
        }
        let ch = bytes[idx];
        if ch == b'"' || ch == b'\'' {
            return parse_open_string(s, idx, ch);
        }
        return None;
    }
    if first == b'"' || first == b'\'' {
        return parse_open_string(s, idx, first);
    }
    None
}

/// Confirm the quote at `quote_pos` opens a still-unclosed string
/// literal and return the text typed so far. Returns `None` if the
/// string is already closed before the cursor — the operator has moved
/// past the first arg.
fn parse_open_string(s: &str, quote_pos: usize, quote: u8) -> Option<ParsedArg<'_>> {
    let bytes = s.as_bytes();
    let content_start = quote_pos + 1;
    let mut j = content_start;
    let mut escape = false;
    while j < bytes.len() {
        if escape {
            escape = false;
            j += 1;
            continue;
        }
        if bytes[j] == b'\\' {
            escape = true;
            j += 1;
            continue;
        }
        if bytes[j] == quote {
            // Closed before the cursor — not a completion target.
            return None;
        }
        j += 1;
    }
    Some(ParsedArg {
        text: &s[content_start..],
        text_offset: content_start,
        needs_opening_quote: false,
    })
}

/// Inside `session.spawn("…"`, `session.spawn{"…"`, `session.spawn(` …
/// the trailing string content is a plugin name, a key name, or an
/// adapter id depending on the call. The Lua VM isn't asked about
/// these — the caches stay authoritative.
fn complete_string_arg(
    prefix: &str,
    prefix_end: usize,
    completer: &ReplCompleter,
) -> Option<Vec<Suggestion>> {
    let ctx = detect_string_arg(&prefix[..prefix_end])?;
    let span = if ctx.needs_opening_quote {
        Span::new(prefix_end, prefix_end)
    } else {
        Span::new(ctx.inside_start, prefix_end)
    };
    let entries = suggestions_for_string_arg(ctx.kind, completer);
    let typed = ctx.inside;
    Some(
        entries
            .into_iter()
            .filter(|(name, _)| name.starts_with(typed))
            .map(|(name, description)| {
                let value = if ctx.needs_opening_quote {
                    format!("\"{name}\"")
                } else {
                    name
                };
                Suggestion {
                    value,
                    description,
                    span,
                    append_whitespace: false,
                    ..Default::default()
                }
            })
            .collect(),
    )
}

/// Materialise the candidate `(name, description)` pairs for a given
/// string-arg kind. Each branch sources from the appropriate cache /
/// static table so completion responsiveness doesn't depend on the
/// Lua mutex.
fn suggestions_for_string_arg(
    kind: StringArgKind,
    completer: &ReplCompleter,
) -> Vec<(String, Option<String>)> {
    match kind {
        StringArgKind::PluginName => completer
            .plugins
            .snapshot()
            .into_iter()
            .map(|name| (name, Some("plugin".into())))
            .collect(),
        StringArgKind::KeyName => KEY_NAMES
            .iter()
            .map(|(name, hint)| ((*name).to_string(), Some((*hint).to_string())))
            .collect(),
        StringArgKind::LocalAdapterId => completer
            .ctx
            .lock()
            .map(|c| {
                c.adapters
                    .iter()
                    .map(|t| (t.id.clone(), Some("adapter id".into())))
                    .collect()
            })
            .unwrap_or_default(),
        StringArgKind::AdapterIdOrAll => {
            let mut out: Vec<(String, Option<String>)> =
                vec![("all".into(), Some("attach every live adapter".into()))];
            let mut seen: BTreeSet<String> = BTreeSet::from([String::from("all")]);
            for id in completer.adapters.snapshot() {
                if seen.insert(id.clone()) {
                    out.push((id, Some("server-side adapter".into())));
                }
            }
            if let Ok(c) = completer.ctx.lock() {
                for tab in &c.adapters {
                    if seen.insert(tab.id.clone()) {
                        out.push((tab.id.clone(), Some("adapter id".into())));
                    }
                }
            }
            out
        }
    }
}

/// List the keys of `lua.globals().<head>` when that's a table. Used by
/// the `module.<TAB>` member-access completion path.
fn members_of(lua: &Lua, head: &str) -> Option<Vec<String>> {
    let table: Table = lua.globals().get(head).ok()?;
    let mut out: Vec<String> = Vec::new();
    for (key, _) in table.pairs::<String, Value>().flatten() {
        out.push(key);
    }
    out.sort();
    Some(out)
}

/// Top-level identifier completion: intersect Lua's full globals view
/// with our explicit `repl_globals` whitelist so stdlib tables don't
/// flood the picker.
fn top_level_completions(
    lua: &Lua,
    whitelist: &HashSet<&'static str>,
    prefix: &str,
    span: Span,
) -> Vec<Suggestion> {
    // Stable display order — both the whitelist iteration and the
    // resulting Suggestion list need to be deterministic for the
    // reedline picker.
    let mut matches: Vec<&str> = whitelist
        .iter()
        .copied()
        .filter(|name| name.starts_with(prefix))
        .collect();
    matches.sort();
    let mut out = Vec::with_capacity(matches.len());
    for name in matches {
        // Confirm the global actually exists — the whitelist could in
        // principle name a binding that failed to install. Cheap check.
        if lua.globals().contains_key(name).unwrap_or(false) {
            let value = if is_nullary(name) {
                format!("{name}()")
            } else {
                name.to_string()
            };
            out.push(Suggestion {
                value,
                description: description_for(name),
                span,
                append_whitespace: false,
                ..Default::default()
            });
        }
    }
    out
}

/// Return the "current token" being typed at the end of `prefix` — the
/// trailing run of identifier-ish characters used to filter completions.
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
    use crate::repl::Framing;
    use crate::repl::lua::LuaRepl;
    use crate::repl::transport::RpcClient;
    use crate::rpc::{RpcServerState, serve_ndjson_with_state};
    use std::time::Duration;

    fn build_test_completer() -> (ReplCompleter, std::thread::JoinHandle<()>) {
        let (c2s_r, c2s_w) = std::io::pipe().expect("pipe c→s");
        let (s2c_r, s2c_w) = std::io::pipe().expect("pipe s→c");
        let state = RpcServerState::new();
        let server = std::thread::spawn(move || {
            let _ = serve_ndjson_with_state(c2s_r, s2c_w, state);
        });
        let client = RpcClient::new(s2c_r, c2s_w, Framing::Ndjson);
        let ctx = Arc::new(Mutex::new(ReplCtx::new()));
        let repl =
            LuaRepl::new(client, Arc::clone(&ctx), Duration::from_secs(5)).expect("LuaRepl::new");
        let completer = ReplCompleter::new(
            ctx,
            PluginCache::new(),
            AdapterCache::new(),
            repl.lua_handle(),
            repl.repl_globals(),
        );
        (completer, server)
    }

    fn ctx_with_adapters(ids: &[(&str, &str)]) -> Arc<Mutex<ReplCtx>> {
        let mut ctx = ReplCtx::new();
        for (id, plugin) in ids {
            ctx.upsert_adapter(id, plugin);
        }
        Arc::new(Mutex::new(ctx))
    }

    fn build_completer_with(
        ctx: Arc<Mutex<ReplCtx>>,
        plugins: PluginCache,
        adapters: AdapterCache,
    ) -> (ReplCompleter, std::thread::JoinHandle<()>) {
        let (c2s_r, c2s_w) = std::io::pipe().expect("pipe c→s");
        let (s2c_r, s2c_w) = std::io::pipe().expect("pipe s→c");
        let state = RpcServerState::new();
        let server = std::thread::spawn(move || {
            let _ = serve_ndjson_with_state(c2s_r, s2c_w, state);
        });
        let client = RpcClient::new(s2c_r, c2s_w, Framing::Ndjson);
        let repl =
            LuaRepl::new(client, Arc::clone(&ctx), Duration::from_secs(5)).expect("LuaRepl::new");
        let completer = ReplCompleter::new(
            ctx,
            plugins,
            adapters,
            repl.lua_handle(),
            repl.repl_globals(),
        );
        (completer, server)
    }

    #[test]
    fn empty_buffer_surfaces_top_level_repl_globals() {
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete("", 0);
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        // Nullary callables come back with `()` appended so Tab lands
        // on an evaluable call; non-nullary identifiers stay bare so
        // the operator can choose between paren and `{...}` sugar.
        for expected in [
            "plugins()",
            "view()",
            "state()",
            "inspect()",
            "session",
            "send",
            "wait",
            "matches",
            "screen_stable",
            "cancel_wait",
            "re",
            "ms",
            "s",
            "turn",
            "transcript",
            "screen",
        ] {
            assert!(
                values.contains(&expected),
                "expected `{expected}` in top-level completions, got {values:?}"
            );
        }
        // Stdlib globals must NOT appear.
        for unwanted in ["string", "math", "table", "io", "os", "_G", "_VERSION"] {
            assert!(
                !values.contains(&unwanted),
                "stdlib `{unwanted}` leaked into top-level completions",
            );
        }
    }

    #[test]
    fn top_level_prefix_filters_to_session() {
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete("ses", 3);
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["session"]);
    }

    #[test]
    fn session_dot_surfaces_member_methods() {
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete("session.", "session.".len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        // `list` and `live` are nullary → completion appends `()`;
        // the rest take arguments so they stay bare.
        for method in ["spawn", "resume", "list()", "live()", "attach", "close"] {
            assert!(
                values.contains(&method),
                "expected `{method}`, got {values:?}"
            );
        }
    }

    #[test]
    fn session_dot_sp_prefix_filters_to_spawn() {
        let (mut completer, _server) = build_test_completer();
        let line = "session.sp";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["spawn"]);
    }

    #[test]
    fn wait_dot_surfaces_matches_and_screen_stable() {
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete("wait.", "wait.".len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(values.contains(&"matches"), "got {values:?}");
        assert!(values.contains(&"screen_stable"), "got {values:?}");
    }

    #[test]
    fn meta_prefix_surfaces_meta_table() {
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete(":", 1);
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        for expected in [":help", ":quit", ":tabs", ":focus", ":notifications on"] {
            assert!(
                values.contains(&expected),
                "expected `{expected}` in meta completions, got {values:?}"
            );
        }
    }

    #[test]
    fn focus_completes_known_adapter_ids() {
        let ctx = ctx_with_adapters(&[("e1", "claude-code"), ("e2", "claude-code")]);
        let (mut completer, _server) =
            build_completer_with(ctx, PluginCache::new(), AdapterCache::new());
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
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
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
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = r#"session.spawn("cla"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["claude-code"]);
    }

    #[test]
    fn attach_completer_suggests_all_and_known_ids() {
        let ctx = ctx_with_adapters(&[("e7", "claude-code")]);
        let (mut completer, _server) =
            build_completer_with(ctx, PluginCache::new(), AdapterCache::new());
        let line = ":attach ";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(values.contains(&"all"), "expected `all`, got {values:?}");
        assert!(values.contains(&"e7"), "expected `e7`, got {values:?}");
    }

    #[test]
    fn attach_completer_surfaces_server_only_ids() {
        let ctx = ctx_with_adapters(&[]);
        let server = AdapterCache::new();
        server.set(vec!["e9".into(), "e10".into()]);
        let (mut completer, _server_thread) = build_completer_with(ctx, PluginCache::new(), server);
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
        let (mut completer, _server_thread) = build_completer_with(ctx, PluginCache::new(), server);
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
        let ctx = ctx_with_adapters(&[]);
        let (mut completer, _server) =
            build_completer_with(ctx, PluginCache::new(), AdapterCache::new());
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
    fn top_level_suggestions_carry_descriptions() {
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete("sess", 4);
        let session = suggestions
            .iter()
            .find(|s| s.value == "session")
            .expect("expected session suggestion");
        let desc = session
            .description
            .as_deref()
            .expect("session suggestion should carry a description");
        assert!(
            desc.contains("spawn") || desc.contains("adapter"),
            "expected meaningful description, got: {desc}"
        );
    }

    #[test]
    fn member_suggestions_carry_descriptions() {
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete("session.", "session.".len());
        let spawn = suggestions
            .iter()
            .find(|s| s.value == "spawn")
            .expect("expected spawn suggestion");
        let desc = spawn
            .description
            .as_deref()
            .expect("session.spawn should carry a description");
        assert!(
            desc.contains("plugin") || desc.contains("spawn"),
            "expected meaningful description, got: {desc}"
        );
    }

    #[test]
    fn send_key_completer_filters_by_partial_prefix() {
        let ctx = ctx_with_adapters(&[]);
        let (mut completer, _server) =
            build_completer_with(ctx, PluginCache::new(), AdapterCache::new());
        let line = r#"send.key("shi"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["shift-tab"]);
    }

    #[test]
    fn spawn_brace_form_completes_plugin_names() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into(), "future-plugin".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = r#"session.spawn{"cla"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["claude-code"]);
        // The replacement span must cover only the typed `cla`, not the
        // opener — otherwise reedline would smash the `{"`.
        let span = suggestions[0].span;
        assert_eq!(span.start, line.len() - 3);
        assert_eq!(span.end, line.len());
    }

    #[test]
    fn spawn_brace_form_with_internal_whitespace_completes() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = r#"session.spawn{ "cla"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["claude-code"]);
    }

    #[test]
    fn spawn_empty_paren_inserts_quoted_plugin_name() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = "session.spawn(";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec![r#""claude-code""#]);
        // Insertion-only span: nothing to replace, just append at cursor.
        let span = suggestions[0].span;
        assert_eq!(span.start, line.len());
        assert_eq!(span.end, line.len());
    }

    #[test]
    fn spawn_empty_brace_inserts_quoted_plugin_name() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = "session.spawn{";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec![r#""claude-code""#]);
    }

    #[test]
    fn spawn_brace_with_internal_ws_and_no_quote_inserts_quoted_name() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = "session.spawn{ ";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec![r#""claude-code""#]);
    }

    #[test]
    fn plugins_describe_string_sugar_with_space_completes() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = r#"plugins.describe "cla"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["claude-code"]);
    }

    #[test]
    fn plugins_describe_string_sugar_no_space_completes() {
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = r#"plugins.describe"cla"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["claude-code"]);
    }

    #[test]
    fn send_key_brace_form_completes() {
        let ctx = ctx_with_adapters(&[]);
        let (mut completer, _server) =
            build_completer_with(ctx, PluginCache::new(), AdapterCache::new());
        let line = r#"send.key{"shi"#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["shift-tab"]);
    }

    #[test]
    fn session_attach_completes_adapter_ids_and_all() {
        let ctx = ctx_with_adapters(&[("e1", "claude-code")]);
        let cache = AdapterCache::new();
        cache.set(vec!["e2".into()]);
        let (mut completer, _server) = build_completer_with(ctx, PluginCache::new(), cache);
        let line = r#"session.attach(""#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(values.contains(&"all"), "expected `all`, got {values:?}");
        assert!(values.contains(&"e1"), "expected `e1`, got {values:?}");
        assert!(values.contains(&"e2"), "expected `e2`, got {values:?}");
    }

    #[test]
    fn session_close_completes_local_adapter_ids() {
        let ctx = ctx_with_adapters(&[("alpha", "claude-code")]);
        let (mut completer, _server) =
            build_completer_with(ctx, PluginCache::new(), AdapterCache::new());
        let line = r#"session.close(""#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["alpha"]);
    }

    #[test]
    fn kwarg_first_arg_does_not_steal_string_completion() {
        // `session.spawn{ rows = 24 ...` — first positional is a kwarg
        // assignment, not a string. The string-arg detector must keep
        // its hands off so identifier completion can do its job.
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = "session.spawn{ rows";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(
            !values.contains(&r#""claude-code""#),
            "kwarg position must not surface quoted plugin names; got {values:?}"
        );
        assert!(
            !values.iter().any(|v| *v == "claude-code"),
            "kwarg position must not surface bare plugin names; got {values:?}"
        );
    }

    #[test]
    fn closed_string_arg_does_not_re_trigger_completion() {
        // After the operator has typed the closing quote, completion
        // for the first arg is over. Surfacing plugin names while the
        // cursor is at a kwarg key would be jarring.
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = r#"session.spawn("claude-code", "#;
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(
            !values.contains(&"claude-code"),
            "post-string position must not re-trigger plugin completion; got {values:?}"
        );
    }

    #[test]
    fn nullary_top_level_globals_complete_with_parens() {
        let (mut completer, _server) = build_test_completer();
        for (line, expected) in [
            ("vi", r#"view()"#),
            ("sta", r#"state()"#),
            ("insp", r#"inspect()"#),
            ("plu", r#"plugins()"#),
        ] {
            let suggestions = completer.complete(line, line.len());
            let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
            assert!(
                values.iter().any(|v| *v == expected),
                "expected `{expected}` from `{line}`, got {values:?}",
            );
        }
    }

    #[test]
    fn nullary_members_complete_with_parens() {
        let (mut completer, _server) = build_test_completer();
        // session.lis<TAB> → list()
        let line = "session.lis";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(
            values.iter().any(|v| *v == "list()"),
            "expected `list()`, got {values:?}",
        );
        // screen.<TAB> → snapshot() in the set
        let line = "screen.";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(
            values.iter().any(|v| *v == "snapshot()"),
            "expected `snapshot()`, got {values:?}",
        );
    }

    #[test]
    fn non_nullary_completions_stay_bare() {
        // `session.spawn` takes a plugin name — appending `()` here
        // would force a paren-form call and break the `{...}` sugar
        // path. Bare identifier completion must stay bare.
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete("session.sp", "session.sp".len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(
            values.iter().any(|v| *v == "spawn"),
            "expected bare `spawn`, got {values:?}",
        );
        assert!(
            !values.iter().any(|v| *v == "spawn()"),
            "non-nullary callables must not auto-`()`; got {values:?}",
        );
    }

    #[test]
    fn transcript_snapshot_stays_bare_to_keep_opts_form_easy() {
        // `transcript.snapshot{ redact = false }` is a real call site.
        // Forcing `()` would make operators backspace before typing `{`.
        let (mut completer, _server) = build_test_completer();
        let suggestions = completer.complete("transcript.", "transcript.".len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(
            values.iter().any(|v| *v == "snapshot"),
            "expected bare `snapshot`, got {values:?}",
        );
        assert!(
            !values.iter().any(|v| *v == "snapshot()"),
            "transcript.snapshot takes opts; must not auto-`()`; got {values:?}",
        );
    }

    #[test]
    fn identifier_tail_does_not_match_call_name() {
        // `xsession.spawn(` must not bind to the `session.spawn` pattern.
        let ctx = ctx_with_adapters(&[]);
        let plugins = PluginCache::new();
        plugins.set(vec!["claude-code".into()]);
        let (mut completer, _server) = build_completer_with(ctx, plugins, AdapterCache::new());
        let line = "xsession.spawn(";
        let suggestions = completer.complete(line, line.len());
        let values: Vec<&str> = suggestions.iter().map(|s| s.value.as_str()).collect();
        assert!(
            !values.contains(&r#""claude-code""#),
            "identifier suffix match must be rejected; got {values:?}"
        );
    }
}
