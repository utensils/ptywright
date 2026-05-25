//! Embedded Lua 5.4 evaluator for the REPL.
//!
//! Each non-meta line typed at the `pty>` prompt is fed through [`LuaRepl::eval`],
//! which loads the line as a Lua chunk and runs it inside a single long-lived
//! `mlua::Lua` VM. Globals like `plugins` (callable table backed by Rust
//! closures) translate into JSON-RPC calls on the bound [`RpcClient`]; return
//! values flow back as Lua tables and are pretty-printed via the bundled
//! `inspect.lua`.
//!
//! The VM ships with the **full Lua 5.4 standard library** — the operator is a
//! trusted local user, same blast radius as a shell. There is no permission
//! gating here (unlike the plugin runtime in [`crate::lua_plugin`]).
//!
//! Layering:
//!
//! 1. [`LuaRepl::new`] constructs the VM, loads `_inspect`, and installs every
//!    REPL-exposed global through small `install_*` helpers.
//! 2. [`LuaRepl::eval`] parses one line ("expression-first, statement-fallback")
//!    and renders the return values into a [`Vec<RenderedValue>`].
//! 3. The caller (the TUI in [`super::tui`]) maps each [`RenderedValue`] back
//!    onto the existing print helpers.
//!
//! This module installs the **entire** REPL-exposed global surface —
//! `plugins`, `session`, `send`, `wait`, `turn`, `cancel_wait`, `state`,
//! `transcript`, `screen`, `view`, `inspect`, and the `re` / `ms` / `s`
//! helpers — through the `install_*` helpers. The TUI in [`super::tui`]
//! only owns line-editing, prompt rendering, and result printing; there
//! is no separate command dispatcher.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{Function, Lua, LuaSerdeExt, MultiValue, RegistryKey, Table, Value};
use serde_json::json;

use super::ctx::ReplCtx;
use super::transport::RpcClient;
use crate::error::{Error, Result};

/// Bundled pretty-printer (kikito/inspect.lua 3.1.0, MIT). Loaded once at
/// [`LuaRepl::new`] and stashed in the Lua registry so every eval can reach
/// it without re-parsing the source.
const INSPECT_SRC: &str = include_str!("inspect.lua");

/// What a single evaluated line produced. The TUI consumes a `Vec<Self>`
/// (one entry per returned MultiValue position) and renders each through
/// the existing print helpers.
#[derive(Debug, Clone)]
pub enum RenderedValue {
    /// Pre-rendered text — either `inspect(value)` output for a table or
    /// the unquoted string itself when the return value is a Lua string.
    Text(String),
    /// Rendered terminal snapshot — the TUI paints it inline with cell
    /// styles using the same `print_screen` helper as the legacy DSL.
    /// Detected via a Lua metatable marker `__pty_type = "screen"`; see
    /// [`install_screen`] and [`render_one`].
    Screen {
        adapter: String,
        snapshot: crate::screen::ScreenSnapshot,
    },
}

/// Outcome of [`LuaRepl::eval`]. A statement-form chunk produces nothing
/// to render; an expression-form chunk produces one entry per MultiValue
/// slot returned.
#[derive(Debug, Clone, Default)]
pub struct EvalResult {
    pub values: Vec<RenderedValue>,
}

/// The REPL's Lua evaluator. One instance is built per `ptywright repl`
/// invocation and lives for the session.
///
/// `lua` is wrapped in a `Mutex` so the completer (when it lands in a later
/// commit) and the validator can introspect the VM from the input thread
/// between `read_line` invocations. The eval path is the only writer; readers
/// only iterate `pairs()` on tables. There is no concurrent contention in
/// practice — reedline drives both serially.
pub struct LuaRepl {
    lua: Arc<Mutex<Lua>>,
    /// Stable handle to the bundled `inspect` function — survives across
    /// eval calls without re-parsing the source.
    inspect_key: RegistryKey,
    /// Names of every global we explicitly installed. Used by the
    /// completer to filter Lua's full globals view down to "things the REPL
    /// is meant to surface" — without this, completion would suggest every
    /// stdlib table (`string`, `math`, `io`, …) at the top level. Wrapped
    /// in `Arc` so the completer and highlighter can share the snapshot
    /// without cloning per keystroke.
    repl_globals: Arc<HashSet<&'static str>>,
    #[allow(dead_code)]
    rpc: Arc<RpcClient>,
    #[allow(dead_code)]
    ctx: Arc<Mutex<ReplCtx>>,
    #[allow(dead_code)]
    rpc_timeout: Duration,
}

impl LuaRepl {
    /// Build a new REPL evaluator. The `rpc` client and `ctx` are shared
    /// references — bindings clone them into their closures.
    pub fn new(
        rpc: Arc<RpcClient>,
        ctx: Arc<Mutex<ReplCtx>>,
        rpc_timeout: Duration,
    ) -> Result<Self> {
        let lua = Lua::new();

        // Load inspect.lua once. The chunk returns a *callable table*
        // (kikito/inspect.lua exports `inspect = {...}` with a `__call`
        // metatable). Wrap it in a thin closure so callers can treat the
        // pretty-printer as a plain `Function`. The closure holds the
        // table as an upvalue, so the table stays alive without us
        // having to leak a global name. `set_name("=inspect")` strips
        // the `[string ...]:N:` prefix from error messages.
        let inspect_table: Value = lua
            .load(INSPECT_SRC)
            .set_name("=inspect")
            .eval()
            .map_err(map_lua_err)?;
        let inspect_fn: Function = lua
            .load("local impl = ...; return function(v) return impl(v) end")
            .call(inspect_table)
            .map_err(map_lua_err)?;
        let inspect_key = lua.create_registry_value(inspect_fn).map_err(map_lua_err)?;

        let mut repl_globals: HashSet<&'static str> = HashSet::new();
        install_plugins(&lua, &rpc, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;
        install_session(&lua, &rpc, &ctx, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;
        install_state(&lua, &rpc, &ctx, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;
        install_send(&lua, &rpc, &ctx, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;
        install_turn(&lua, &rpc, &ctx, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;
        install_wait(&lua, &rpc, &ctx, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;
        install_matcher_constructors(&lua, &mut repl_globals).map_err(map_lua_err)?;
        install_cancel_wait(&lua, &rpc, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;
        install_duration_helpers(&lua, &mut repl_globals).map_err(map_lua_err)?;
        install_transcript(&lua, &rpc, &ctx, rpc_timeout, &mut repl_globals)
            .map_err(map_lua_err)?;
        install_screen(&lua, &rpc, &ctx, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;
        install_inspect(&lua, &rpc, &ctx, rpc_timeout, &mut repl_globals).map_err(map_lua_err)?;

        Ok(Self {
            lua: Arc::new(Mutex::new(lua)),
            inspect_key,
            repl_globals: Arc::new(repl_globals),
            rpc,
            ctx,
            rpc_timeout,
        })
    }

    /// Borrow the Lua mutex. The completer and validator hold their own
    /// `Arc` to it so the read-eval-print loop can share a single VM
    /// across input pathways. Reedline serialises completer/validator/eval
    /// on the input thread, so contention is theoretical.
    pub fn lua_handle(&self) -> Arc<Mutex<Lua>> {
        Arc::clone(&self.lua)
    }

    /// Snapshot of the REPL-exposed global names (the whitelist used to
    /// filter top-level completions and to colour identifiers in the
    /// highlighter). Stable for the REPL's lifetime.
    pub fn repl_globals(&self) -> Arc<HashSet<&'static str>> {
        Arc::clone(&self.repl_globals)
    }

    /// Evaluate one line. Tries the line as an expression first (so
    /// `plugins()` returns a value to inspect); falls back to statement
    /// form (`local x = 1`, `for i = 1, 3 do … end`) if that fails to parse.
    ///
    /// Returns `Err` if the chunk parses but the runtime raises an error,
    /// or if both expression and statement forms fail to parse.
    pub fn eval(&self, line: &str) -> Result<EvalResult> {
        let lua = self.lua.lock().expect("repl lua mutex");

        // Expression form: wrap as `return <line>` so `plugins()` produces
        // a MultiValue we can render.
        let as_expr = lua
            .load(format!("return {line}"))
            .set_name("=stdin")
            .into_function();
        if let Ok(func) = as_expr {
            let values: MultiValue = func.call(()).map_err(map_lua_err)?;
            return self.render_values(&lua, values);
        }

        // Statement form: assignments, control flow, side-effect-only calls.
        lua.load(line)
            .set_name("=stdin")
            .exec()
            .map_err(map_lua_err)?;
        Ok(EvalResult::default())
    }

    fn render_values(&self, lua: &Lua, values: MultiValue) -> Result<EvalResult> {
        let inspect: Function = lua.registry_value(&self.inspect_key).map_err(map_lua_err)?;
        let mut rendered = Vec::with_capacity(values.len());
        for value in values {
            rendered.push(render_one(lua, &inspect, value)?);
        }
        Ok(EvalResult { values: rendered })
    }
}

/// Convert one mlua [`Value`] into a [`RenderedValue`] the TUI can paint.
///
/// `lua` is passed in (rather than taken from `self.lua`) because callers
/// already hold the lua mutex — re-locking would deadlock.
fn render_one(lua: &Lua, inspect: &Function, value: Value) -> Result<RenderedValue> {
    match value {
        Value::Nil => Ok(RenderedValue::Text("nil".to_string())),
        Value::Boolean(b) => Ok(RenderedValue::Text(b.to_string())),
        Value::Integer(n) => Ok(RenderedValue::Text(n.to_string())),
        Value::Number(n) => Ok(RenderedValue::Text(n.to_string())),
        Value::String(s) => Ok(RenderedValue::Text(
            s.to_str().map_err(map_lua_err)?.to_string(),
        )),
        Value::Table(ref tbl) => {
            // Detect the `__pty_type = "screen"` metatable marker and
            // decode the table back into a `ScreenSnapshot` so the TUI
            // can render it with the styled inline view. Metatable tags
            // don't appear in `pairs()`, so the user-facing table shape
            // is unaffected.
            if let Some(mt) = tbl.metatable()
                && let Ok(kind) = mt.get::<String>("__pty_type")
                && kind == "screen"
            {
                let adapter: String = mt.get("__pty_adapter").map_err(map_lua_err)?;
                // Temporarily detach the metatable so the serde-via-Lua
                // deserializer doesn't choke on the marker fields. We
                // restore it afterwards so a value that gets printed
                // *and* assigned to a variable still carries the tag.
                // Both set_metatable calls propagate — silently dropping
                // a restore failure would leave a printed-then-reused
                // ScreenSnapshot table looking like a plain table on the
                // second render, which is exactly the kind of silent
                // state corruption that breaks `s = screen.snapshot(); s`.
                let restored = mt.clone();
                tbl.set_metatable(None).map_err(map_lua_err)?;
                let decode_result: mlua::Result<crate::screen::ScreenSnapshot> =
                    lua.from_value(Value::Table(tbl.clone()));
                tbl.set_metatable(Some(restored)).map_err(map_lua_err)?;
                let snapshot = decode_result.map_err(map_lua_err)?;
                return Ok(RenderedValue::Screen { adapter, snapshot });
            }
            // Plain table — delegate to inspect.lua for pretty-printing.
            let text: String = inspect
                .call(Value::Table(tbl.clone()))
                .map_err(map_lua_err)?;
            Ok(RenderedValue::Text(text))
        }
        Value::Function(_) => {
            // Bare-function results almost always mean the operator
            // typed an identifier without the trailing `()` — `view`
            // instead of `view()`. inspect.lua would render this as
            // `<function 1>` which is useless and slightly mysterious;
            // a hint nudges them toward the intended call without
            // auto-invoking (that would surprise anyone who *did* mean
            // to return a function reference).
            Ok(RenderedValue::Text(
                "<function> · did you mean to call it with `()`?".to_string(),
            ))
        }
        other => {
            // Userdata, threads, etc. — delegate to inspect.lua.
            let text: String = inspect.call(other).map_err(map_lua_err)?;
            Ok(RenderedValue::Text(text))
        }
    }
}

/// reedline `Validator` impl: ask Lua whether the input is a complete chunk.
///
/// Without this, reedline submits as soon as the user hits Enter, which
/// makes multi-line expressions like `for i = 1, 3 do session.spawn("x") end`
/// impossible to type without semicolons. The implementation:
///
/// 1. Lines starting with `:` are meta commands — never multi-line.
/// 2. Otherwise, try the input as `return <line>` (expression form). If
///    that parses, the line is complete.
/// 3. Failing that, try the input as a statement. If it parses, complete.
/// 4. If parsing fails with `mlua::Error::SyntaxError { incomplete_input: true }`,
///    return [`reedline::ValidationResult::Incomplete`] so reedline keeps
///    accepting input on the continuation prompt.
/// 5. Any other parse error: return `Complete` so the eval path surfaces
///    the actual error to the operator (instead of leaving them stuck on
///    a continuation prompt for a permanent syntax error).
pub struct LuaValidator {
    lua: Arc<Mutex<mlua::Lua>>,
}

impl LuaValidator {
    pub fn new(lua: Arc<Mutex<mlua::Lua>>) -> Self {
        Self { lua }
    }
}

impl reedline::Validator for LuaValidator {
    fn validate(&self, line: &str) -> reedline::ValidationResult {
        let trimmed = line.trim_start();
        if trimmed.starts_with(':') || trimmed.is_empty() {
            return reedline::ValidationResult::Complete;
        }
        let lua = match self.lua.lock() {
            Ok(g) => g,
            Err(_) => return reedline::ValidationResult::Complete,
        };
        let expr_form = lua
            .load(format!("return {line}"))
            .set_name("=stdin")
            .into_function();
        let stmt_form_result = || lua.load(line).set_name("=stdin").into_function();
        match expr_form {
            Ok(_) => reedline::ValidationResult::Complete,
            Err(mlua::Error::SyntaxError {
                incomplete_input: true,
                ..
            }) => {
                // The expression form is mid-typed. Check the statement
                // form too — if *it* parses cleanly, treat the input as
                // a complete statement (e.g. `local x = { ...` is a valid
                // expression that's incomplete, but `do ... end` is a
                // complete statement that isn't a valid expression).
                match stmt_form_result() {
                    Ok(_) => reedline::ValidationResult::Complete,
                    _ => reedline::ValidationResult::Incomplete,
                }
            }
            Err(_) => {
                // Expression form failed for some non-incomplete reason
                // (e.g. the line starts with `local`, which can't be an
                // expression). Defer to the statement form.
                match stmt_form_result() {
                    Ok(_) => reedline::ValidationResult::Complete,
                    Err(mlua::Error::SyntaxError {
                        incomplete_input: true,
                        ..
                    }) => reedline::ValidationResult::Incomplete,
                    Err(_) => reedline::ValidationResult::Complete,
                }
            }
        }
    }
}

/// Install the `plugins` callable table.
///
/// `plugins()` (via `__call`) maps to `adapter.list`; `plugins.describe("…")`
/// maps to `plugin.describe`. Both are read-only registry queries with no
/// adapter dependency, which is why this is the first binding to land — it
/// exercises the whole eval → JSON conversion → inspect-render pipeline
/// without touching `ReplCtx` mutation paths.
fn install_plugins(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let plugins = lua.create_table()?;

    let rpc_describe = Arc::clone(rpc);
    plugins.set(
        "describe",
        lua.create_function(move |lua, name: String| {
            let result = rpc_describe
                .call("plugin.describe", json!({ "plugin": name }), timeout)
                .map_err(rpc_to_lua_err)?;
            lua.to_value(&result)
        })?,
    )?;

    let mt = lua.create_table()?;
    let rpc_list = Arc::clone(rpc);
    mt.set(
        "__call",
        lua.create_function(move |lua, _args: MultiValue| {
            // `_args` includes the implicit `self` (the `plugins` table).
            // We ignore it and dispatch the read-only call.
            let result = rpc_list
                .call("adapter.list", json!({}), timeout)
                .map_err(rpc_to_lua_err)?;
            lua.to_value(&result)
        })?,
    )?;
    plugins.set_metatable(Some(mt))?;

    lua.globals().set("plugins", plugins)?;
    repl_globals.insert("plugins");
    Ok(())
}

/// Install `session.*` bindings (`spawn`, `resume`, `list`, `live`, `attach`,
/// `close`) as a top-level `session` table.
///
/// Each binding accepts both `session.spawn("name", { rows = 24 })` and the
/// Lua-sugar `session.spawn{ "name", rows = 24 }` forms via [`read_call`].
/// `session.spawn` / `session.resume` populate `ctx.adapters` + `ctx.focus`
/// on success through the shared [`adopt_started_adapter`] helper.
fn install_session(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let session = lua.create_table()?;

    // session.spawn(plugin, opts?) → adapter.start
    let rpc_spawn = Arc::clone(rpc);
    let ctx_spawn = Arc::clone(ctx);
    session.set(
        "spawn",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, opts) = read_call(lua, args)?;
            let plugin = expect_string(&positional, "session.spawn", "plugin name")?;
            let params = build_adapter_start_params(&plugin, &opts)?;
            let result = rpc_spawn
                .call("adapter.start", params, timeout)
                .map_err(rpc_to_lua_err)?;
            adopt_started_adapter(&ctx_spawn, &plugin, &result);
            lua.to_value(&result)
        })?,
    )?;

    // session.resume(plugin, opts?) → adapter.resume
    let rpc_resume = Arc::clone(rpc);
    let ctx_resume = Arc::clone(ctx);
    session.set(
        "resume",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, opts) = read_call(lua, args)?;
            let plugin = expect_string(&positional, "session.resume", "plugin name")?;
            let mut params = build_adapter_start_params(&plugin, &opts)?;
            let prior = resolve_prior_alias(&opts)?;
            if let Some(prior_id) = prior.as_deref()
                && let serde_json::Value::Object(ref mut map) = params
            {
                map.insert(
                    "prior_adapter".to_string(),
                    serde_json::Value::String(prior_id.to_string()),
                );
            }
            let result = rpc_resume
                .call("adapter.resume", params, timeout)
                .map_err(rpc_to_lua_err)?;
            if let Some(prior_id) = prior.as_deref() {
                ctx_resume
                    .lock()
                    .expect("ctx mutex")
                    .remove_adapter(prior_id);
            }
            adopt_started_adapter(&ctx_resume, &plugin, &result);
            lua.to_value(&result)
        })?,
    )?;

    // session.list() — purely local, reads ctx.adapters.
    let ctx_list = Arc::clone(ctx);
    session.set(
        "list",
        lua.create_function(move |lua, _: MultiValue| {
            let ctx = ctx_list.lock().expect("ctx mutex");
            let out = lua.create_table()?;
            for (i, tab) in ctx.adapters.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("id", tab.id.clone())?;
                entry.set("plugin", tab.plugin.clone())?;
                if let Some(label) = &tab.state_label {
                    entry.set("state", label.clone())?;
                }
                if ctx.focus.as_deref() == Some(tab.id.as_str()) {
                    entry.set("focused", true)?;
                }
                out.raw_set(i + 1, entry)?;
            }
            Ok(out)
        })?,
    )?;

    // session.live() → adapter.live
    let rpc_live = Arc::clone(rpc);
    session.set(
        "live",
        lua.create_function(move |lua, _: MultiValue| {
            let result = rpc_live
                .call("adapter.live", json!({}), timeout)
                .map_err(rpc_to_lua_err)?;
            lua.to_value(&result)
        })?,
    )?;

    // session.attach("id" | "all") — adapter.state (single) or adapter.live (all),
    // populates local tabs + focus accordingly. The screen-render-on-attach
    // behaviour from the legacy DSL lands in a later commit alongside the
    // screen.snapshot binding.
    let rpc_attach = Arc::clone(rpc);
    let ctx_attach = Arc::clone(ctx);
    session.set(
        "attach",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, _opts) = read_call(lua, args)?;
            let target = match positional.first() {
                None => "all".to_string(),
                Some(Value::String(s)) => s.to_str()?.to_string(),
                Some(other) => {
                    return Err(rpc_err(format!(
                        "session.attach expects an adapter id or \"all\", got {other:?}"
                    )));
                }
            };
            if target == "all" {
                let live = rpc_attach
                    .call("adapter.live", json!({}), timeout)
                    .map_err(rpc_to_lua_err)?;
                let mut attached = Vec::new();
                if let Some(entries) = live.get("adapters").and_then(|v| v.as_array()) {
                    let mut ctx = ctx_attach.lock().expect("ctx mutex");
                    for entry in entries {
                        if entry
                            .get("finished")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        let id = match entry.get("adapter").and_then(serde_json::Value::as_str) {
                            Some(id) if !id.is_empty() => id.to_string(),
                            _ => continue,
                        };
                        let plugin = entry
                            .get("plugin")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("?")
                            .to_string();
                        ctx.upsert_adapter(&id, &plugin);
                        attached.push(id);
                    }
                    if let Some(last) = attached.last() {
                        ctx.focus = Some(last.clone());
                    }
                }
                lua.to_value(&json!({ "attached": attached }))
            } else {
                let state_resp = rpc_attach
                    .call("adapter.state", json!({ "adapter": target }), timeout)
                    .map_err(rpc_to_lua_err)?;
                let plugin = state_resp
                    .get("plugin")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                {
                    let mut ctx = ctx_attach.lock().expect("ctx mutex");
                    ctx.upsert_adapter(&target, &plugin);
                    if let Some(label) = state_resp
                        .get("state")
                        .and_then(|s| s.get("state"))
                        .and_then(serde_json::Value::as_str)
                    {
                        ctx.set_state_label(&target, Some(label.to_string()));
                    }
                    ctx.focus = Some(target.clone());
                }
                lua.to_value(&state_resp)
            }
        })?,
    )?;

    // session.close(id?) → adapter.close
    let rpc_close = Arc::clone(rpc);
    let ctx_close = Arc::clone(ctx);
    session.set(
        "close",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, _opts) = read_call(lua, args)?;
            let adapter = match positional.first() {
                Some(Value::String(s)) => s.to_str()?.to_string(),
                Some(other) => {
                    return Err(rpc_err(format!(
                        "session.close expects a string adapter id; got {other:?}"
                    )));
                }
                None => focus_required(&ctx_close)?,
            };
            let result = rpc_close
                .call("adapter.close", json!({ "adapter": adapter }), timeout)
                .map_err(rpc_to_lua_err)?;
            ctx_close
                .lock()
                .expect("ctx mutex")
                .remove_adapter(&adapter);
            lua.to_value(&result)
        })?,
    )?;

    lua.globals().set("session", session)?;
    repl_globals.insert("session");
    Ok(())
}

/// Install the top-level `state()` global.
///
/// `state` is a free function rather than a method on `session` because it
/// always targets the focused adapter — there is no `session.state(id)` form.
fn install_state(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let rpc = Arc::clone(rpc);
    let ctx = Arc::clone(ctx);
    let f = lua.create_function(move |lua, _: MultiValue| {
        let adapter = focus_required(&ctx)?;
        let result = rpc
            .call("adapter.state", json!({ "adapter": adapter }), timeout)
            .map_err(rpc_to_lua_err)?;
        lua.to_value(&result)
    })?;
    lua.globals().set("state", f)?;
    repl_globals.insert("state");
    Ok(())
}

/// Install `send.*` bindings (`text`, `key`, `intent`) as a top-level
/// `send` table. All three require a focused adapter and route through
/// `adapter.send` with `{ adapter, intent, params }`.
fn install_send(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let send = lua.create_table()?;

    // send.text("…") — wires as intent=send_prompt with `{prompt}`.
    // The plugin's send_prompt intent reads input.prompt, so sending
    // `{text: …}` would silently bracketed-paste an empty string —
    // this is the wire-shape contract a contract test pins down.
    let rpc_text = Arc::clone(rpc);
    let ctx_text = Arc::clone(ctx);
    send.set(
        "text",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, _opts) = read_call(lua, args)?;
            let text = expect_string(&positional, "send.text", "prompt string")?;
            let adapter = focus_required(&ctx_text)?;
            let result = rpc_text
                .call(
                    "adapter.send",
                    json!({
                        "adapter": adapter,
                        "intent": "send_prompt",
                        "params": { "prompt": text },
                    }),
                    timeout,
                )
                .map_err(rpc_to_lua_err)?;
            lua.to_value(&result)
        })?,
    )?;

    // send.key("enter") — intent=key with `{key}`.
    let rpc_key = Arc::clone(rpc);
    let ctx_key = Arc::clone(ctx);
    send.set(
        "key",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, _opts) = read_call(lua, args)?;
            let key = expect_string(&positional, "send.key", "key name")?;
            let adapter = focus_required(&ctx_key)?;
            let result = rpc_key
                .call(
                    "adapter.send",
                    json!({
                        "adapter": adapter,
                        "intent": "key",
                        "params": { "key": key },
                    }),
                    timeout,
                )
                .map_err(rpc_to_lua_err)?;
            lua.to_value(&result)
        })?,
    )?;

    // send.intent("name", { k = v, ... }) — escape hatch for any plugin
    // intent. The opts table is converted verbatim to JSON params; no
    // schema check (the plugin gets to validate).
    let rpc_intent = Arc::clone(rpc);
    let ctx_intent = Arc::clone(ctx);
    send.set(
        "intent",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, opts) = read_call(lua, args)?;
            let intent = expect_string(&positional, "send.intent", "intent name")?;
            let params: serde_json::Value = lua.from_value(Value::Table(opts))?;
            let adapter = focus_required(&ctx_intent)?;
            let result = rpc_intent
                .call(
                    "adapter.send",
                    json!({
                        "adapter": adapter,
                        "intent": intent,
                        "params": params,
                    }),
                    timeout,
                )
                .map_err(rpc_to_lua_err)?;
            lua.to_value(&result)
        })?,
    )?;

    lua.globals().set("send", send)?;
    repl_globals.insert("send");
    Ok(())
}

/// Install the top-level `turn` global — atomic `adapter.turn` (send
/// + wait_turn_matcher in one shot).
///
/// Surface: `turn("intent", { ...send_params, wait = <matcher>, wait_intent = "...", timeout = ms(N) })`.
///
/// Reserved opts keys:
/// * `wait` — a matcher value (tagged table from `matches(...)` /
///   `screen_stable(...)`, or a bare string auto-promoted to a regex).
/// * `wait_intent` — override the matcher function name (default
///   `wait_turn_matcher`). Used when a plugin owns multiple matchers.
/// * `timeout` — integer milliseconds. Bound to the wait leg only;
///   the send leg is bounded by the per-call `RPC_TIMEOUT`.
///
/// All non-reserved keys flow into `send.params`.
fn install_turn(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let rpc = Arc::clone(rpc);
    let ctx = Arc::clone(ctx);
    let f = lua.create_function(move |lua, args: MultiValue| {
        let (positional, opts) = read_call(lua, args)?;
        let intent = expect_string(&positional, "turn", "intent name")?;
        // Validate the timeout shape *before* looking up focus so a typo
        // surfaces a "expected non-negative milliseconds" error instead
        // of "no focused adapter".
        if let Some(timeout_ms) = opts.get::<Option<i64>>("timeout")?
            && timeout_ms < 0
        {
            return Err(rpc_err(format!(
                "turn(timeout=…) expected non-negative milliseconds, got {timeout_ms}"
            )));
        }
        let adapter = focus_required(&ctx)?;

        // Build send.params from opts minus the three reserved keys.
        let mut send_params = serde_json::Map::new();
        for pair in opts.clone().pairs::<String, Value>() {
            let (key, value) = pair?;
            if matches!(key.as_str(), "wait" | "wait_intent" | "timeout") {
                continue;
            }
            let json_value: serde_json::Value = lua.from_value(value)?;
            send_params.insert(key, json_value);
        }

        // Build the wait sub-object.
        let mut wait_obj = serde_json::Map::new();
        if let Some(wait_value) = opts.get::<Option<Value>>("wait")? {
            let (wait_intent_resolved, wait_params) = resolve_matcher(wait_value)?;
            wait_obj.insert(
                "intent".to_string(),
                serde_json::Value::String(wait_intent_resolved),
            );
            wait_obj.insert("params".to_string(), wait_params);
        } else if let Some(wait_intent) = opts.get::<Option<String>>("wait_intent")? {
            wait_obj.insert("intent".to_string(), serde_json::Value::String(wait_intent));
        }
        if let Some(timeout_ms) = opts.get::<Option<i64>>("timeout")? {
            // Already validated for sign at the top of the closure.
            wait_obj.insert(
                "timeout_ms".to_string(),
                serde_json::Value::from(timeout_ms),
            );
        }

        let mut params = serde_json::Map::new();
        params.insert("adapter".to_string(), serde_json::Value::String(adapter));
        params.insert(
            "send".to_string(),
            json!({ "intent": intent, "params": serde_json::Value::Object(send_params) }),
        );
        if !wait_obj.is_empty() {
            params.insert("wait".to_string(), serde_json::Value::Object(wait_obj));
        }

        let result = rpc
            .call("adapter.turn", serde_json::Value::Object(params), timeout)
            .map_err(rpc_to_lua_err)?;
        lua.to_value(&result)
    })?;
    lua.globals().set("turn", f)?;
    repl_globals.insert("turn");
    Ok(())
}

/// Install the `wait` callable table.
///
/// Two forms:
/// * `wait(matcher, { timeout = ms(2000), wait_id = "…" })` — invoked via
///   the table's `__call` metamethod. Resolves the matcher via
///   [`resolve_matcher`] (tagged tables or bare-string-auto-promote) and
///   issues `adapter.wait`.
/// * `wait.matches("…", { timeout = ms(2000) })` and
///   `wait.screen_stable(ms, { timeout = ms(2000) })` — shorthand that
///   skips the matcher constructor and goes straight to `adapter.wait`
///   with the `wait_turn_matcher` intent.
///
/// `pairs()` semantics are unaffected by the metatable (Lua's default
/// `pairs` iterates the underlying table). The completer can still
/// introspect the named keys (`matches`, `screen_stable`) without
/// `__call` getting in the way.
fn install_wait(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let wait_tbl = lua.create_table()?;

    // wait.matches(pattern, { timeout = ms(...) })
    let rpc_matches = Arc::clone(rpc);
    let ctx_matches = Arc::clone(ctx);
    wait_tbl.set(
        "matches",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, opts) = read_call(lua, args)?;
            let pattern = expect_string(&positional, "wait.matches", "pattern string")?;
            wait_dispatch(
                lua,
                &rpc_matches,
                &ctx_matches,
                json!({ "pattern": pattern }),
                &opts,
                timeout,
            )
        })?,
    )?;

    // wait.screen_stable(ms, { timeout = ms(...) })
    let rpc_stable = Arc::clone(rpc);
    let ctx_stable = Arc::clone(ctx);
    wait_tbl.set(
        "screen_stable",
        lua.create_function(move |lua, args: MultiValue| {
            let (positional, opts) = read_call(lua, args)?;
            let stable_ms = expect_i64(&positional, "wait.screen_stable", "milliseconds")?;
            wait_dispatch(
                lua,
                &rpc_stable,
                &ctx_stable,
                json!({ "stable_ms": stable_ms }),
                &opts,
                timeout,
            )
        })?,
    )?;

    // Metatable's __call: wait(matcher, opts?). Argument parsing is
    // hand-rolled rather than going through `read_call` because the
    // matcher is *itself* a table — read_call would mistake the matcher
    // for a Lua-sugar combined call and split it into positional/kwargs.
    // The `wait{...}` sugar form has no meaning for wait anyway (the
    // first arg is mandatory and not a string), so this binding takes
    // arguments by position only.
    let mt = lua.create_table()?;
    let rpc_call = Arc::clone(rpc);
    let ctx_call = Arc::clone(ctx);
    mt.set(
        "__call",
        lua.create_function(move |lua, args: MultiValue| {
            // First arg is the wait table itself (__call convention) — skip it.
            let mut iter = args.into_iter();
            let _self = iter.next();
            let matcher_value = iter.next().ok_or_else(|| {
                rpc_err(
                    "wait() expects a matcher: wait(matches(\"…\")) or wait(screen_stable(ms(250)))"
                        .to_string(),
                )
            })?;
            let opts = match iter.next() {
                Some(Value::Table(t)) => t,
                Some(other) => {
                    return Err(rpc_err(format!(
                        "wait() opts must be a table; got {other:?}"
                    )));
                }
                None => lua.create_table()?,
            };
            let (intent, params) = resolve_matcher(matcher_value)?;
            wait_dispatch_with_intent(lua, &rpc_call, &ctx_call, &intent, params, &opts, timeout)
        })?,
    )?;
    wait_tbl.set_metatable(Some(mt))?;

    lua.globals().set("wait", wait_tbl)?;
    repl_globals.insert("wait");
    Ok(())
}

/// Issue `adapter.wait` with the canonical `wait_turn_matcher` intent.
/// Used by the `wait.matches` / `wait.screen_stable` shorthand methods,
/// which always target the canonical matcher.
fn wait_dispatch(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    matcher_params: serde_json::Value,
    opts: &Table,
    timeout: Duration,
) -> mlua::Result<Value> {
    wait_dispatch_with_intent(
        lua,
        rpc,
        ctx,
        "wait_turn_matcher",
        matcher_params,
        opts,
        timeout,
    )
}

/// Issue `adapter.wait` with a caller-chosen matcher intent. Used by
/// `wait(...)` via `__call`, where the matcher constructor may select an
/// intent other than `wait_turn_matcher`.
fn wait_dispatch_with_intent(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    intent: &str,
    matcher_params: serde_json::Value,
    opts: &Table,
    timeout: Duration,
) -> mlua::Result<Value> {
    // Validate the timeout shape *before* looking up focus so a bad
    // number surfaces a precise error instead of "no focused adapter".
    let timeout_override = match opts.get::<Option<i64>>("timeout")? {
        Some(ms) if ms < 0 => {
            return Err(rpc_err(format!(
                "wait(timeout=…) expected non-negative milliseconds, got {ms}"
            )));
        }
        other => other,
    };
    let adapter = focus_required(ctx)?;
    let mut params = serde_json::Map::new();
    params.insert("adapter".to_string(), serde_json::Value::String(adapter));
    params.insert(
        "intent".to_string(),
        serde_json::Value::String(intent.to_string()),
    );
    params.insert("params".to_string(), matcher_params);
    if let Some(timeout_ms) = timeout_override {
        params.insert(
            "timeout_ms".to_string(),
            serde_json::Value::from(timeout_ms),
        );
    }
    if let Some(wait_id) = opts.get::<Option<String>>("wait_id")? {
        params.insert("wait_id".to_string(), serde_json::Value::String(wait_id));
    }
    let result = rpc
        .call("adapter.wait", serde_json::Value::Object(params), timeout)
        .map_err(rpc_to_lua_err)?;
    lua.to_value(&result)
}

/// Install the matcher *constructors* `matches` and `screen_stable` as
/// top-level globals. Each returns a tagged table that [`resolve_matcher`]
/// recognises — there is no RPC traffic until `wait(...)` / `turn(...)`
/// resolves the matcher.
fn install_matcher_constructors(
    lua: &Lua,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    lua.globals().set(
        "matches",
        lua.create_function(|lua, args: MultiValue| {
            let (positional, _opts) = read_call(lua, args)?;
            let pattern = expect_string(&positional, "matches", "regex pattern")?;
            let out = lua.create_table()?;
            out.set("__pty_matcher", "regex")?;
            out.set("pattern", pattern)?;
            Ok(out)
        })?,
    )?;
    repl_globals.insert("matches");

    lua.globals().set(
        "screen_stable",
        lua.create_function(|lua, args: MultiValue| {
            let (positional, _opts) = read_call(lua, args)?;
            let stable_ms = expect_i64(&positional, "screen_stable", "milliseconds")?;
            let out = lua.create_table()?;
            out.set("__pty_matcher", "stable")?;
            out.set("stable_ms", stable_ms)?;
            Ok(out)
        })?,
    )?;
    repl_globals.insert("screen_stable");
    Ok(())
}

/// Install `transcript.snapshot(opts?)` — dispatches `adapter.transcript`
/// against the focused adapter. Recognised opts:
///
/// * `redact` (bool, default `true`) — when true, the server applies its
///   redaction policy before returning the transcript. Operators who
///   need the unredacted view (debugging the policy, say) pass
///   `{ redact = false }`.
fn install_transcript(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let transcript = lua.create_table()?;
    let rpc = Arc::clone(rpc);
    let ctx = Arc::clone(ctx);
    transcript.set(
        "snapshot",
        lua.create_function(move |lua, args: MultiValue| {
            let (_positional, opts) = read_call(lua, args)?;
            let redact = opts.get::<Option<bool>>("redact")?.unwrap_or(true);
            let adapter = focus_required(&ctx)?;
            let result = rpc
                .call(
                    "adapter.transcript",
                    json!({ "adapter": adapter, "redact": redact }),
                    timeout,
                )
                .map_err(rpc_to_lua_err)?;
            lua.to_value(&result)
        })?,
    )?;
    lua.globals().set("transcript", transcript)?;
    repl_globals.insert("transcript");
    Ok(())
}

/// Install `screen.snapshot()` (with the `view()` alias) for fetching the
/// focused adapter's current PTY state. Always passes `redact = false` —
/// the REPL view should match what the agent really sees; the redacted
/// copy is available via `:rpc adapter.snapshot { adapter = "…", redact = true }`.
///
/// Return values are tagged with `__pty_type = "screen"` in a metatable so
/// [`render_one`] knows to paint them with the styled inline renderer
/// instead of running them through `inspect`.
fn install_screen(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let screen = lua.create_table()?;
    let rpc_snap = Arc::clone(rpc);
    let ctx_snap = Arc::clone(ctx);
    let snapshot_fn = lua.create_function(move |lua, _: MultiValue| {
        let adapter = focus_required(&ctx_snap)?;
        let result = rpc_snap
            .call(
                "adapter.snapshot",
                json!({ "adapter": adapter, "redact": false }),
                timeout,
            )
            .map_err(rpc_to_lua_err)?;
        tag_screen_value(lua, &adapter, &result)
    })?;
    screen.set("snapshot", snapshot_fn.clone())?;
    lua.globals().set("screen", screen)?;
    repl_globals.insert("screen");

    // `view()` — top-level alias. Same body, same RPC, same tagging.
    let rpc_view = Arc::clone(rpc);
    let ctx_view = Arc::clone(ctx);
    lua.globals().set(
        "view",
        lua.create_function(move |lua, _: MultiValue| {
            let adapter = focus_required(&ctx_view)?;
            let result = rpc_view
                .call(
                    "adapter.snapshot",
                    json!({ "adapter": adapter, "redact": false }),
                    timeout,
                )
                .map_err(rpc_to_lua_err)?;
            tag_screen_value(lua, &adapter, &result)
        })?,
    )?;
    repl_globals.insert("view");
    Ok(())
}

/// Install the top-level `inspect()` global — `adapter.inspect`'s
/// diagnostic dump for the focused adapter.
///
/// **Naming note:** this binding shadows what a Lua user might expect to
/// be a pretty-printer named `inspect`. The pretty-printer is the
/// `_inspect` table stashed in the Lua registry; it is invoked by the
/// REPL's print path automatically for any non-screen return value. Users
/// who want to pretty-print a value manually should call `print(_)` —
/// the REPL's `inspect` global is reserved for the adapter diagnostic.
fn install_inspect(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    ctx: &Arc<Mutex<ReplCtx>>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let rpc = Arc::clone(rpc);
    let ctx = Arc::clone(ctx);
    let f = lua.create_function(move |lua, _: MultiValue| {
        let adapter = focus_required(&ctx)?;
        let result = rpc
            .call("adapter.inspect", json!({ "adapter": adapter }), timeout)
            .map_err(rpc_to_lua_err)?;
        lua.to_value(&result)
    })?;
    lua.globals().set("inspect", f)?;
    repl_globals.insert("inspect");
    Ok(())
}

/// Convert a JSON snapshot response into a Lua table whose metatable
/// flags it as a screen value. Used by `screen.snapshot()` / `view()` /
/// (future) `session.attach`.
fn tag_screen_value(
    lua: &Lua,
    adapter: &str,
    snapshot_json: &serde_json::Value,
) -> mlua::Result<Value> {
    let value = lua.to_value(snapshot_json)?;
    if let Value::Table(tbl) = &value {
        let mt = lua.create_table()?;
        mt.set("__pty_type", "screen")?;
        mt.set("__pty_adapter", adapter.to_string())?;
        tbl.set_metatable(Some(mt))?;
    }
    Ok(value)
}

/// Install `cancel_wait("id")` — breaks loose a still-in-flight wait that
/// was issued with `wait(..., { wait_id = "…" })`, typically from another
/// REPL session or background script.
fn install_cancel_wait(
    lua: &Lua,
    rpc: &Arc<RpcClient>,
    timeout: Duration,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    let rpc = Arc::clone(rpc);
    let f = lua.create_function(move |lua, args: MultiValue| {
        let (positional, _opts) = read_call(lua, args)?;
        let wait_id = expect_string(&positional, "cancel_wait", "wait id")?;
        let result = rpc
            .call(
                "adapter.cancel_wait",
                json!({ "wait_id": wait_id }),
                timeout,
            )
            .map_err(rpc_to_lua_err)?;
        lua.to_value(&result)
    })?;
    lua.globals().set("cancel_wait", f)?;
    repl_globals.insert("cancel_wait");
    Ok(())
}

/// Install the three duration / regex documentation helpers.
///
/// * `re(s)` — identity. `wait(matches(re("^❯")))` reads more naturally
///   than `wait(matches("^❯"))` when the operator wants to flag "this is
///   a regex, not just a string" at the call site.
/// * `ms(n)` — identity. Same idea for milliseconds: `screen_stable(ms(250))`.
/// * `s(n)` — converts seconds to milliseconds. `screen_stable(s(2))` →
///   2000 ms. This is the only one that does actual work.
///
/// All three accept either an integer or a number; non-finite floats and
/// negative inputs to `s` are passed through (the wait dispatcher will
/// reject them at use site).
fn install_duration_helpers(
    lua: &Lua,
    repl_globals: &mut HashSet<&'static str>,
) -> mlua::Result<()> {
    lua.globals()
        .set("re", lua.create_function(|_, value: String| Ok(value))?)?;
    repl_globals.insert("re");

    lua.globals()
        .set("ms", lua.create_function(|_, value: i64| Ok(value))?)?;
    repl_globals.insert("ms");

    lua.globals().set(
        "s",
        lua.create_function(|_, value: i64| Ok(value.saturating_mul(1000)))?,
    )?;
    repl_globals.insert("s");
    Ok(())
}

/// Pull an integer positional out of [`read_call`]'s output. Used by the
/// matcher constructors (`screen_stable`, `wait.screen_stable`) for the
/// milliseconds argument.
fn expect_i64(positional: &[Value], label: &str, what: &str) -> mlua::Result<i64> {
    match positional.first() {
        Some(Value::Integer(n)) => Ok(*n),
        Some(Value::Number(n)) => Ok(*n as i64),
        Some(other) => Err(rpc_err(format!(
            "{label} expects {what} (integer); got {other:?}"
        ))),
        None => Err(rpc_err(format!("{label} expects {what}"))),
    }
}

/// Resolve a `wait = …` value into a matcher intent + params pair.
///
/// Accepts:
/// * A tagged table from `matches(...)` / `screen_stable(...)` —
///   distinguished by the `__pty_matcher` key.
/// * A bare string — auto-promoted to a regex matcher (preserves the
///   legacy `wait("^❯")` shorthand).
///
/// Errors on anything else; the message names the surface so the
/// operator knows what `wait=` accepts.
fn resolve_matcher(value: Value) -> mlua::Result<(String, serde_json::Value)> {
    match value {
        Value::Table(tbl) => {
            let kind: Option<String> = tbl.get("__pty_matcher")?;
            match kind.as_deref() {
                Some("regex") => {
                    let pattern: String = tbl.get("pattern")?;
                    Ok((
                        "wait_turn_matcher".to_string(),
                        json!({ "pattern": pattern }),
                    ))
                }
                Some("stable") => {
                    let stable_ms: i64 = tbl.get("stable_ms")?;
                    Ok((
                        "wait_turn_matcher".to_string(),
                        json!({ "stable_ms": stable_ms }),
                    ))
                }
                Some(other) => Err(rpc_err(format!(
                    "wait= matcher tag `{other}` is not recognised; use matches(...) or screen_stable(...)"
                ))),
                None => Err(rpc_err(
                    "wait= got a plain table; expected matches(...) or screen_stable(...)"
                        .to_string(),
                )),
            }
        }
        Value::String(s) => {
            let pattern = s.to_str()?.to_string();
            Ok((
                "wait_turn_matcher".to_string(),
                json!({ "pattern": pattern }),
            ))
        }
        other => Err(rpc_err(format!(
            "wait= expected matches(...) / screen_stable(...) / a regex string; got {other:?}"
        ))),
    }
}

/// Read the focused adapter id, returning a friendly Lua error when no
/// adapter is focused.
fn focus_required(ctx: &Mutex<ReplCtx>) -> mlua::Result<String> {
    ctx.lock()
        .expect("ctx mutex")
        .focus
        .as_deref()
        .map(str::to_string)
        .ok_or_else(|| {
            rpc_err("no focused adapter — spawn one with `session.spawn(...)` first".to_string())
        })
}

/// Adopt a freshly-started adapter into the local tab list + focus pointer.
fn adopt_started_adapter(
    ctx: &Arc<Mutex<ReplCtx>>,
    requested_plugin: &str,
    result: &serde_json::Value,
) {
    let Some(adapter) = result.get("adapter").and_then(serde_json::Value::as_str) else {
        return;
    };
    let response_plugin = result
        .get("plugin")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(requested_plugin);
    let mut ctx = ctx.lock().expect("ctx mutex");
    ctx.upsert_adapter(adapter, response_plugin);
    ctx.focus = Some(adapter.to_string());
}

/// Pull a string positional out of [`read_call`]'s output, with a friendly
/// "expected ..." Lua error on miss.
fn expect_string(positional: &[Value], label: &str, what: &str) -> mlua::Result<String> {
    match positional.first() {
        Some(Value::String(s)) => Ok(s.to_str()?.to_string()),
        Some(other) => Err(rpc_err(format!(
            "{label} expects a string {what}; got {other:?}"
        ))),
        None => Err(rpc_err(format!("{label} expects a string {what}"))),
    }
}

/// Resolve the `prior_adapter` kwarg on `session.resume`, honouring the
/// documented `prior` alias.
///
/// `Table::get::<Option<String>>(key)` returns `Ok(None)` when the key
/// is absent, so a naive `or_else` only fires on type errors and would
/// silently ignore the alias. The match below falls through on absence
/// (the common case) while still propagating a type error from the
/// canonical key — operators who write `prior_adapter = 7` get a Lua
/// error instead of having the value quietly replaced by whatever
/// `prior` resolves to.
fn resolve_prior_alias(opts: &Table) -> mlua::Result<Option<String>> {
    match opts.get::<Option<String>>("prior_adapter")? {
        Some(value) => Ok(Some(value)),
        None => opts.get::<Option<String>>("prior"),
    }
}

/// Construct an `adapter.start` / `adapter.resume` params object from the
/// plugin name + the trailing opts table. Optional kwargs supported:
///
/// * `program` (string), `cwd` (string)
/// * `args` (string list), `env` (string→string map)
/// * `rows`, `cols`, `pixel_width`, `pixel_height` (u16)
///
/// Unknown opts keys are rejected to avoid silently swallowing typos like
/// `session.spawn(..., colss = 80)`.
fn build_adapter_start_params(plugin: &str, opts: &Table) -> mlua::Result<serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert(
        "plugin".to_string(),
        serde_json::Value::String(plugin.to_string()),
    );
    if let Some(program) = opts.get::<Option<String>>("program")? {
        map.insert("program".to_string(), serde_json::Value::String(program));
    }
    if let Some(cwd) = opts.get::<Option<String>>("cwd")? {
        map.insert("cwd".to_string(), serde_json::Value::String(cwd));
    }
    if opts.contains_key("args")? {
        let args: Vec<String> = opts.get("args")?;
        map.insert(
            "args".to_string(),
            serde_json::Value::Array(args.into_iter().map(serde_json::Value::String).collect()),
        );
    }
    if opts.contains_key("env")? {
        let env_tbl: Table = opts.get("env")?;
        let mut env_obj = serde_json::Map::new();
        for pair in env_tbl.clone().pairs::<String, String>() {
            let (k, v) = pair?;
            env_obj.insert(k, serde_json::Value::String(v));
        }
        map.insert("env".to_string(), serde_json::Value::Object(env_obj));
    }
    for name in ["rows", "cols", "pixel_width", "pixel_height"] {
        if let Some(value) = opts.get::<Option<i64>>(name)? {
            let n = u16::try_from(value)
                .map_err(|_| rpc_err(format!("{name}= expected 0..65535, got {value}")))?;
            map.insert(name.to_string(), serde_json::Value::from(n));
        }
    }
    // Reject unknown opts keys (except the resume-only "prior_adapter"/"prior",
    // which `session.resume` handles separately).
    let known: HashSet<&'static str> = HashSet::from([
        "program",
        "cwd",
        "args",
        "env",
        "rows",
        "cols",
        "pixel_width",
        "pixel_height",
        "prior_adapter",
        "prior",
    ]);
    for pair in opts.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(s) = key {
            let s = s.to_str()?.to_string();
            if !known.contains(s.as_str()) {
                return Err(rpc_err(format!(
                    "unknown session option `{s}`; expected one of: {}",
                    known.iter().copied().collect::<Vec<_>>().join(", ")
                )));
            }
        }
    }
    Ok(serde_json::Value::Object(map))
}

/// Build an mlua external-error from a string message. The error round-trips
/// through `Function::call` and surfaces back to the REPL as a clean
/// `stdin:N: <msg>` line via `format_lua_error`.
fn rpc_err(message: String) -> mlua::Error {
    mlua::Error::external(Error::Rpc(message))
}

/// Cooperative kwarg-shape destructure for binding closures.
///
/// The REPL accepts two call shapes: `f("name", { k = v })` (positional +
/// trailing options table) and the Lua sugar `f{ "name", k = v }` (single
/// table with both array and hash parts). This helper normalises both into
/// the same `(positional, opts)` pair so each binding only writes one shape
/// of parsing code.
///
/// Returns:
/// * `positional` — array-part values, in order.
/// * `opts` — the kwargs table; an empty table when none were given.
///
/// Rules:
/// * If `args.len() == 1` and `args[0]` is a table with at least one string
///   key (a hash part), treat the whole table as the combined form — split
///   into the integer-indexed entries (positional) plus the remaining string
///   keys (opts).
/// * Otherwise, all leading non-table args become positional. If the **last**
///   arg is a table, it becomes opts. Tables in the middle stay positional.
pub fn read_call(lua: &Lua, args: MultiValue) -> mlua::Result<(Vec<Value>, Table)> {
    let mut iter = args.into_iter();
    let first = iter.next();
    let rest: Vec<Value> = iter.collect();

    // `f{ "name", k = v }` — single-table combined form. Treat *any*
    // lone-table call as the combined form, even when only an array
    // part is present (`f{ "name" }`) or the table is empty (`f{}`).
    // Without this, brace-sugar with no kwargs falls through to the
    // generic positional path and leaves the table itself as
    // `positional[0]`, which every binding that expects a string /
    // number / matcher then rejects with `expected a string`.
    //
    // Bindings that genuinely take a table as their first positional
    // (e.g. `wait(matcher)`, where the matcher is a tagged table)
    // bypass `read_call` and parse `MultiValue` directly so the
    // unpack here cannot harm them.
    if rest.is_empty()
        && let Some(Value::Table(tbl)) = first.as_ref()
    {
        let positional = take_array_part(tbl)?;
        let opts = filter_string_keys(lua, tbl)?;
        return Ok((positional, opts));
    }

    // Otherwise: scan positional + optional trailing-table opts.
    let mut positional: Vec<Value> = Vec::new();
    if let Some(first) = first {
        positional.push(first);
    }
    positional.extend(rest);

    let trailing_table = match positional.last() {
        Some(Value::Table(tbl)) => Some(tbl.clone()),
        _ => None,
    };
    let opts = if let Some(tbl) = trailing_table {
        // Pop the trailing table iff it has any string keys, OR if it has
        // no integer keys (which would otherwise leave us with no way to
        // pass an "opts" table that happens to be empty — we read the
        // operator's intent from the *position* in that case).
        let has_strings = table_has_string_key(&tbl)?;
        let has_ints = table_has_integer_key(&tbl)?;
        if has_strings || !has_ints {
            positional.pop();
            tbl
        } else {
            lua.create_table()?
        }
    } else {
        lua.create_table()?
    };

    Ok((positional, opts))
}

fn table_has_string_key(tbl: &Table) -> mlua::Result<bool> {
    for pair in tbl.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if matches!(key, Value::String(_)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn table_has_integer_key(tbl: &Table) -> mlua::Result<bool> {
    for pair in tbl.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if matches!(key, Value::Integer(_)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn take_array_part(tbl: &Table) -> mlua::Result<Vec<Value>> {
    let len = tbl.raw_len();
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        out.push(tbl.raw_get(i)?);
    }
    Ok(out)
}

fn filter_string_keys(lua: &Lua, tbl: &Table) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    for pair in tbl.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        if let Value::String(s) = key {
            out.set(s, value)?;
        }
    }
    Ok(out)
}

/// Format an mlua error for surfacing through the REPL's red `✗` line.
/// `set_name("=stdin")` already strips the `[string …]:N:` chunk prefix,
/// so we only sanitise the common `runtime error: ` prefix that mlua
/// adds for non-syntax errors.
pub fn format_lua_error(error: &Error) -> String {
    let text = error.to_string();
    text.strip_prefix("runtime error: ")
        .unwrap_or(&text)
        .to_string()
}

/// Convert a `crate::error::Error` raised by an RPC call into an mlua error
/// so it propagates up through `Function::call` instead of being swallowed.
fn rpc_to_lua_err(error: Error) -> mlua::Error {
    mlua::Error::external(error)
}

/// Convert mlua errors into the crate's [`Error`] enum.
fn map_lua_err(error: mlua::Error) -> Error {
    Error::Lua(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::Framing;
    use crate::repl::ctx::ReplCtx;
    use crate::repl::transport::RpcClient;
    use crate::rpc::{RpcServerState, serve_ndjson_with_state};

    fn spawn_test_repl() -> (LuaRepl, std::thread::JoinHandle<()>) {
        let (c2s_r, c2s_w) = std::io::pipe().expect("pipe c→s");
        let (s2c_r, s2c_w) = std::io::pipe().expect("pipe s→c");
        let state = RpcServerState::new();
        let server = std::thread::spawn(move || {
            let _ = serve_ndjson_with_state(c2s_r, s2c_w, state);
        });
        let client = RpcClient::new(s2c_r, c2s_w, Framing::Ndjson);
        let ctx = Arc::new(Mutex::new(ReplCtx::new()));
        let repl = LuaRepl::new(client, ctx, Duration::from_secs(5)).expect("LuaRepl::new");
        (repl, server)
    }

    #[test]
    fn eval_returns_expression_value() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("1 + 2").expect("eval 1+2");
        assert_eq!(result.values.len(), 1);
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "3");
    }

    #[test]
    fn eval_runs_statement() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("local x = 1 + 1").expect("statement");
        // Statement form produces no rendered values.
        assert!(result.values.is_empty());
    }

    #[test]
    fn eval_preserves_globals_across_lines() {
        let (repl, _server) = spawn_test_repl();
        repl.eval("answer = 42").expect("assign global");
        let result = repl.eval("answer").expect("read global");
        assert_eq!(result.values.len(), 1);
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "42");
    }

    #[test]
    fn eval_strings_render_unquoted() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("'hello'").expect("eval string");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn resolve_prior_alias_picks_canonical_key_first() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("table");
        opts.set("prior_adapter", "e1").expect("set canonical");
        opts.set("prior", "e2").expect("set alias");
        let resolved = resolve_prior_alias(&opts).expect("resolve");
        assert_eq!(resolved.as_deref(), Some("e1"));
    }

    #[test]
    fn resolve_prior_alias_falls_through_to_alias_when_canonical_absent() {
        // The bug `codex review` flagged: previously `or_else` only fired
        // on type errors, so `prior = "e1"` alone never reached the wire.
        // Lock the fix in with an explicit alias-only fixture.
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("table");
        opts.set("prior", "e1").expect("set alias");
        let resolved = resolve_prior_alias(&opts).expect("resolve");
        assert_eq!(resolved.as_deref(), Some("e1"));
    }

    #[test]
    fn resolve_prior_alias_returns_none_when_neither_key_present() {
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("table");
        let resolved = resolve_prior_alias(&opts).expect("resolve");
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_prior_alias_propagates_type_error_on_canonical_key() {
        // A bad type on `prior_adapter` must surface as a Lua error —
        // it must NOT silently fall through to `prior`, which would
        // mask the operator's typo. Use a table (which Lua cannot
        // coerce to a string) rather than a number (Lua silently
        // converts those via `lua_tostring`).
        let lua = mlua::Lua::new();
        let opts = lua.create_table().expect("table");
        let nested = lua.create_table().expect("nested table");
        opts.set("prior_adapter", nested).expect("set bad type");
        opts.set("prior", "e1").expect("set alias");
        assert!(
            resolve_prior_alias(&opts).is_err(),
            "non-stringy type on canonical key must error, not fall through",
        );
    }

    #[test]
    fn bare_function_value_renders_with_call_hint() {
        // Typing `view` (no parens) is almost always a mistake — the
        // operator wanted to call it. The default inspect.lua output is
        // a bare `<function 1>` which is unhelpful. We catch that arm
        // and emit a friendlier hint pointing at the `()` syntax.
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("view").expect("eval bare view");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert!(
            text.contains("<function>") && text.contains("()"),
            "expected friendly hint, got: {text}",
        );
        // Whatever we emit, it must not be the raw inspect output.
        assert!(
            !text.contains("<function 1>"),
            "raw inspect output leaked through: {text}",
        );
    }

    #[test]
    fn eval_tables_pretty_print_via_inspect() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("{ a = 1, b = 2 }").expect("eval table");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        // inspect.lua wraps tables in braces with sorted/aligned keys.
        assert!(text.contains("a = 1"), "expected `a = 1` in: {text}");
        assert!(text.contains("b = 2"), "expected `b = 2` in: {text}");
    }

    #[test]
    fn eval_returns_error_for_unknown_identifier_call() {
        let (repl, _server) = spawn_test_repl();
        // `not_a_real_global` is intentionally absent; calling it should
        // raise a "nil value" error that surfaces back to the operator.
        let err = repl.eval("not_a_real_global()").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nil") || msg.contains("not_a_real_global"),
            "expected nil-call error mentioning the missing global, got: {msg}",
        );
    }

    #[test]
    fn plugins_describe_round_trips_through_plugin_describe_rpc() {
        // `plugins.describe("claude-code")` must call `plugin.describe`
        // with the right wire shape and surface the resulting catalog
        // — at minimum the canonical send_prompt intent and the
        // wait_turn_matcher wait function.
        let (repl, _server) = spawn_test_repl();
        let result = repl
            .eval(r#"plugins.describe("claude-code")"#)
            .expect("plugins.describe");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert!(
            text.contains("claude-code"),
            "plugin name should be present: {text}"
        );
        assert!(
            text.contains("send_prompt"),
            "send_prompt intent missing: {text}"
        );
        assert!(
            text.contains("wait_turn_matcher"),
            "wait_turn_matcher missing: {text}"
        );
    }

    #[test]
    fn plugins_call_dispatches_to_adapter_list() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("plugins()").expect("plugins() must succeed");
        assert_eq!(result.values.len(), 1);
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        // The default RpcServerState ships with claude-code as a built-in
        // plugin, so the response table must mention it.
        assert!(
            text.contains("claude-code"),
            "expected claude-code in: {text}",
        );
    }

    #[test]
    fn read_call_combined_form_splits_positional_and_kwargs() {
        // `f{ "x", k = 1 }` — Lua sugar combined form.
        let lua = Lua::new();
        let test = lua
            .create_function(|lua, args: MultiValue| {
                let (positional, opts) = read_call(lua, args)?;
                let pos_len = positional.len();
                let k: Option<i64> = opts.get("k")?;
                Ok((pos_len, k))
            })
            .expect("create_function");
        lua.globals().set("f", test).expect("set f");
        let (pos_len, k): (usize, Option<i64>) =
            lua.load(r#"return f{ "x", k = 1 }"#).eval().expect("eval");
        assert_eq!(pos_len, 1);
        assert_eq!(k, Some(1));
    }

    #[test]
    fn read_call_explicit_opts_table_form() {
        // `f("x", { k = 1 })` — explicit trailing-table form.
        let lua = Lua::new();
        let test = lua
            .create_function(|lua, args: MultiValue| {
                let (positional, opts) = read_call(lua, args)?;
                let pos_len = positional.len();
                let k: Option<i64> = opts.get("k")?;
                Ok((pos_len, k))
            })
            .expect("create_function");
        lua.globals().set("f", test).expect("set f");
        let (pos_len, k): (usize, Option<i64>) = lua
            .load(r#"return f("x", { k = 1 })"#)
            .eval()
            .expect("eval");
        assert_eq!(pos_len, 1);
        assert_eq!(k, Some(1));
    }

    #[test]
    fn read_call_no_opts_yields_empty_table() {
        let lua = Lua::new();
        let test = lua
            .create_function(|lua, args: MultiValue| {
                let (positional, opts) = read_call(lua, args)?;
                Ok((positional.len(), opts.is_empty()))
            })
            .expect("create_function");
        lua.globals().set("f", test).expect("set f");
        let (pos_len, opts_empty): (usize, bool) =
            lua.load(r#"return f("x", "y")"#).eval().expect("eval");
        assert_eq!(pos_len, 2);
        assert!(opts_empty, "no kwargs should yield an empty opts table");
    }

    #[test]
    fn read_call_brace_form_with_only_array_part_is_split() {
        // The bug `codex review` flagged: `f{"x"}` previously fell
        // through to the generic positional path and left the table
        // itself as `positional[0]`, breaking `session.spawn{"name"}`,
        // `session.attach{"all"}`, and `send.intent{"approve"}`. Pin
        // the unpack semantics so the regression cannot creep back.
        let lua = Lua::new();
        let test = lua
            .create_function(|lua, args: MultiValue| {
                let (positional, opts) = read_call(lua, args)?;
                let first: Option<String> = match positional.first() {
                    Some(Value::String(s)) => Some(s.to_str()?.to_string()),
                    _ => None,
                };
                Ok((positional.len(), first, opts.is_empty()))
            })
            .expect("create_function");
        lua.globals().set("f", test).expect("set f");
        let (pos_len, first, opts_empty): (usize, Option<String>, bool) =
            lua.load(r#"return f{ "x" }"#).eval().expect("eval");
        assert_eq!(pos_len, 1, "array part must become positional[0]");
        assert_eq!(first.as_deref(), Some("x"));
        assert!(opts_empty, "no string keys → opts is empty");
    }

    #[test]
    fn read_call_brace_form_with_empty_table_yields_empty_positional() {
        // `f{}` — an empty Lua sugar table. Should produce empty
        // positional + empty opts, not a single Table positional.
        let lua = Lua::new();
        let test = lua
            .create_function(|lua, args: MultiValue| {
                let (positional, opts) = read_call(lua, args)?;
                Ok((positional.len(), opts.is_empty()))
            })
            .expect("create_function");
        lua.globals().set("f", test).expect("set f");
        let (pos_len, opts_empty): (usize, bool) = lua.load(r#"return f{}"#).eval().expect("eval");
        assert_eq!(pos_len, 0, "empty brace-form must yield no positional");
        assert!(opts_empty, "empty brace-form must yield no opts");
    }

    #[test]
    fn repl_globals_records_installed_names() {
        let (repl, _server) = spawn_test_repl();
        assert!(repl.repl_globals.contains("plugins"));
        assert!(repl.repl_globals.contains("session"));
        assert!(repl.repl_globals.contains("state"));
        // Stdlib globals are NOT in the whitelist — they're available to
        // the user but the completer should not surface them at the top
        // level. Verifying via a sentinel keeps a future refactor honest.
        assert!(!repl.repl_globals.contains("string"));
        assert!(!repl.repl_globals.contains("math"));
    }

    #[test]
    fn session_list_returns_empty_table_with_no_adapters() {
        let (repl, _server) = spawn_test_repl();
        // session.list() is purely local; works without ever talking to RPC.
        let result = repl.eval("session.list()").expect("session.list()");
        assert_eq!(result.values.len(), 1);
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        // inspect of an empty table renders something like "{}" or with no
        // entries — accept any reasonable shape.
        assert!(text.contains('{') && text.contains('}'), "got: {text}");
    }

    #[test]
    fn session_live_round_trips_through_rpc() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("session.live()").expect("session.live()");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        // adapter.live always returns { adapters = [...] }, even when empty.
        assert!(text.contains("adapters"), "got: {text}");
    }

    #[test]
    fn state_without_focus_returns_friendly_error() {
        let (repl, _server) = spawn_test_repl();
        let err = repl.eval("state()").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("no focused adapter"),
            "expected friendly focus error, got: {message}"
        );
    }

    #[test]
    fn session_spawn_rejects_unknown_kwarg() {
        let (repl, _server) = spawn_test_repl();
        // The "colss" typo should not silently flow into the server as
        // an unknown field — the binding catches it before the RPC call.
        let err = repl
            .eval(r#"session.spawn("claude-code", { colss = 80 })"#)
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("unknown session option"),
            "expected unknown-option error, got: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_spawn_close_cycle_updates_ctx() {
        let (repl, _server) = spawn_test_repl();
        // Use trailing-table form. The /bin/sh override keeps the test
        // hermetic — no real claude binary needed.
        let result = repl
            .eval(r#"session.spawn("claude-code", { program = "/bin/sh" })"#)
            .expect("session.spawn");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert!(text.contains("adapter"), "expected adapter id in: {text}");

        // ctx must now contain exactly one adapter and have it focused.
        {
            let ctx = repl.ctx.lock().expect("ctx mutex");
            assert_eq!(ctx.adapters.len(), 1);
            assert!(ctx.focus.is_some());
        }

        // state() should succeed against the focused adapter.
        repl.eval("state()").expect("state() with focus");

        // Close clears the tab and focus.
        repl.eval("session.close()").expect("session.close()");
        let ctx = repl.ctx.lock().expect("ctx mutex");
        assert!(ctx.adapters.is_empty(), "close should drop the tab");
        assert!(ctx.focus.is_none(), "close should clear focus");
    }

    #[cfg(unix)]
    #[test]
    fn session_spawn_accepts_lua_sugar_combined_form() {
        // `session.spawn{ "claude-code", program = "/bin/sh" }` — Lua's
        // function-call sugar. The combined-table form must split into
        // positional + kwargs the same way as the explicit two-arg form.
        let (repl, _server) = spawn_test_repl();
        repl.eval(r#"session.spawn{ "claude-code", program = "/bin/sh" }"#)
            .expect("session.spawn sugar form");
        {
            // Scope the lock so the subsequent eval can re-acquire it
            // inside the close binding — holding ctx across an eval call
            // deadlocks because the binding closure locks ctx too.
            let ctx = repl.ctx.lock().expect("ctx mutex");
            assert_eq!(ctx.adapters.len(), 1);
        }
        repl.eval("session.close()").ok();
    }

    #[cfg(unix)]
    #[test]
    fn session_spawn_passes_args_and_env_through() {
        let (repl, _server) = spawn_test_repl();
        // args/env round-trip via the explicit start-params build path.
        repl.eval(
            r#"session.spawn("claude-code", {
                program = "/bin/sh",
                args = { "-lc", "printf ok" },
                env = { NO_COLOR = "1" },
                rows = 24,
                cols = 80
            })"#,
        )
        .expect("session.spawn with args+env");
        repl.eval("session.close()").ok();
    }

    // ---- send.* + turn ---------------------------------------------------

    #[test]
    fn send_text_without_focus_errors_friendly() {
        let (repl, _server) = spawn_test_repl();
        let err = repl.eval(r#"send.text("hello")"#).unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "expected focus-required error, got: {err}"
        );
    }

    #[test]
    fn send_intent_kwargs_convert_to_json_params() {
        // Drive a syntactic round-trip without a real adapter: we expect
        // the binding to fail at the RPC layer ("no focused adapter"),
        // not at the Lua → JSON conversion. The error message confirms
        // the call shape parsed.
        let (repl, _server) = spawn_test_repl();
        let err = repl
            .eval(r#"send.intent("foo", { a = 1, b = "two", c = true })"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "expected focus-required error, got: {err}"
        );
    }

    #[test]
    fn resolve_matcher_accepts_regex_tagged_table() {
        let lua = Lua::new();
        let tbl: Table = lua
            .load(r#"return { __pty_matcher = "regex", pattern = "^x" }"#)
            .eval()
            .expect("matcher table");
        let (intent, params) = resolve_matcher(Value::Table(tbl)).expect("regex matcher");
        assert_eq!(intent, "wait_turn_matcher");
        assert_eq!(params["pattern"], "^x");
    }

    #[test]
    fn resolve_matcher_accepts_stable_tagged_table() {
        let lua = Lua::new();
        let tbl: Table = lua
            .load(r#"return { __pty_matcher = "stable", stable_ms = 250 }"#)
            .eval()
            .expect("matcher table");
        let (intent, params) = resolve_matcher(Value::Table(tbl)).expect("stable matcher");
        assert_eq!(intent, "wait_turn_matcher");
        assert_eq!(params["stable_ms"], 250);
    }

    #[test]
    fn resolve_matcher_auto_promotes_bare_string() {
        let lua = Lua::new();
        let s: Value = lua.load(r#"return "^❯""#).eval().expect("string value");
        let (intent, params) = resolve_matcher(s).expect("string matcher");
        assert_eq!(intent, "wait_turn_matcher");
        assert_eq!(params["pattern"], "^❯");
    }

    #[test]
    fn resolve_matcher_rejects_plain_table() {
        let lua = Lua::new();
        let tbl: Table = lua
            .load(r#"return { not_a_matcher = true }"#)
            .eval()
            .expect("plain table");
        let err = resolve_matcher(Value::Table(tbl)).unwrap_err();
        assert!(
            err.to_string().contains("plain table"),
            "expected plain-table error, got: {err}"
        );
    }

    #[test]
    fn turn_rejects_negative_timeout() {
        let (repl, _server) = spawn_test_repl();
        let err = repl
            .eval(r#"turn("send_prompt", { prompt = "hi", timeout = -1 })"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("non-negative"),
            "expected negative-timeout error, got: {err}"
        );
    }

    // ---- wait + matcher constructors + duration helpers -----------------

    #[test]
    fn matches_returns_regex_tagged_table() {
        let (repl, _server) = spawn_test_repl();
        // Evaluate the constructor and inspect the resulting table.
        let result = repl
            .eval(r#"matches("^x").__pty_matcher"#)
            .expect("matches tag");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "regex");
        let result = repl.eval(r#"matches("^x").pattern"#).expect("pattern");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "^x");
    }

    #[test]
    fn screen_stable_returns_stable_tagged_table() {
        let (repl, _server) = spawn_test_repl();
        let result = repl
            .eval("screen_stable(250).__pty_matcher")
            .expect("stable tag");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "stable");
        let result = repl
            .eval("screen_stable(250).stable_ms")
            .expect("stable_ms");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "250");
    }

    #[test]
    fn re_helper_is_identity() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval(r#"re("^anything$")"#).expect("re identity");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "^anything$");
    }

    #[test]
    fn ms_helper_is_identity() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("ms(250)").expect("ms identity");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "250");
    }

    #[test]
    fn s_helper_converts_seconds_to_milliseconds() {
        let (repl, _server) = spawn_test_repl();
        let result = repl.eval("s(2)").expect("s converts");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(text, "2000");
    }

    #[test]
    fn wait_requires_focus() {
        let (repl, _server) = spawn_test_repl();
        let err = repl.eval(r#"wait(matches("^x"))"#).unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "expected focus-required error, got: {err}"
        );
    }

    #[test]
    fn wait_call_form_uses_metatable_call() {
        // The `wait(matcher)` form goes through the metatable's __call.
        // We can't fully exercise it without a focused adapter, but we
        // can prove the call shape lands on the focus-required check —
        // meaning __call resolved and matcher extraction succeeded.
        let (repl, _server) = spawn_test_repl();
        let err = repl
            .eval(r#"wait(matches("^x"), { timeout = ms(100) })"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "expected focus error after __call resolution, got: {err}"
        );
    }

    #[test]
    fn wait_shorthand_methods_dispatch_to_wait_turn_matcher() {
        let (repl, _server) = spawn_test_repl();
        // Same idea: confirm the call shape resolves down to the
        // focus-required check, not a "not a function" error.
        let err = repl.eval(r#"wait.matches("^x")"#).unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "wait.matches should resolve, got: {err}"
        );
        let err = repl.eval("wait.screen_stable(250)").unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "wait.screen_stable should resolve, got: {err}"
        );
    }

    #[test]
    fn cancel_wait_dispatches_to_adapter_cancel_wait() {
        let (repl, _server) = spawn_test_repl();
        // No matching wait_id on the server — we expect either a
        // not-found error or a no-op JSON response. Either way the
        // call must reach the server.
        let _ = repl.eval(r#"cancel_wait("nonexistent")"#);
    }

    #[test]
    fn negative_wait_timeout_is_rejected() {
        let (repl, _server) = spawn_test_repl();
        let err = repl
            .eval(r#"wait.matches("^x", { timeout = -1 })"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("non-negative"),
            "expected negative-timeout rejection, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wait_matches_full_loop_times_out_against_unmatched_pattern() {
        // End-to-end smoke: spawn /bin/sh, ask wait.matches to find a
        // pattern it'll never see, with a tight 200ms timeout. The
        // server-side matcher loop must surface -32001 — proving the
        // entire wait_dispatch path is intact.
        let (repl, _server) = spawn_test_repl();
        repl.eval(
            r#"session.spawn("claude-code", {
                program = "/bin/sh",
                args = { "-lc", "printf ready; cat" }
            })"#,
        )
        .expect("spawn fixture sh");
        let result = repl.eval(r#"wait.matches("never-matches", { timeout = 200 })"#);
        match result {
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("-32001") || message.contains("matcher"),
                    "expected matcher timeout from adapter.wait, got `{message}`"
                );
            }
            Ok(other) => panic!("expected -32001 matcher timeout, got {other:?}"),
        }
        repl.eval("session.close()").ok();
    }

    // ---- transcript / screen / inspect ----------------------------------

    #[test]
    fn transcript_snapshot_requires_focus() {
        let (repl, _server) = spawn_test_repl();
        let err = repl.eval("transcript.snapshot()").unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "expected focus-required error, got: {err}"
        );
    }

    #[test]
    fn screen_snapshot_requires_focus() {
        let (repl, _server) = spawn_test_repl();
        let err = repl.eval("screen.snapshot()").unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "expected focus-required error, got: {err}"
        );
        let err = repl.eval("view()").unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "view() should mirror screen.snapshot(), got: {err}"
        );
    }

    #[test]
    fn inspect_requires_focus() {
        let (repl, _server) = spawn_test_repl();
        let err = repl.eval("inspect()").unwrap_err();
        assert!(
            err.to_string().contains("no focused adapter"),
            "expected focus-required error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn screen_snapshot_returns_screen_tagged_value() {
        // Spawn, then call screen.snapshot() — the binding must return a
        // value that the render path classifies as RenderedValue::Screen.
        let (repl, _server) = spawn_test_repl();
        repl.eval(r#"session.spawn("claude-code", { program = "/bin/sh" })"#)
            .expect("spawn fixture sh");
        let result = repl.eval("screen.snapshot()").expect("screen.snapshot");
        assert_eq!(result.values.len(), 1);
        match &result.values[0] {
            RenderedValue::Screen {
                adapter,
                snapshot: _,
            } => {
                assert!(!adapter.is_empty(), "expected non-empty adapter id");
            }
            other => panic!("expected RenderedValue::Screen, got: {other:?}"),
        }
        repl.eval("session.close()").ok();
    }

    #[cfg(unix)]
    #[test]
    fn view_alias_returns_screen_tagged_value() {
        let (repl, _server) = spawn_test_repl();
        repl.eval(r#"session.spawn("claude-code", { program = "/bin/sh" })"#)
            .expect("spawn fixture sh");
        let result = repl.eval("view()").expect("view()");
        assert!(
            matches!(&result.values[0], RenderedValue::Screen { .. }),
            "view() should be tagged as a screen value",
        );
        repl.eval("session.close()").ok();
    }

    #[cfg(unix)]
    #[test]
    fn screen_metatable_marker_does_not_leak_into_pairs() {
        // Operators iterating `for k, v in pairs(s) do ... end` should NOT
        // see `__pty_type` or `__pty_adapter` — those live in the metatable.
        // The pure-Lua check is folded into a single-expression eval so
        // the existing `return <expr>` form handles it without needing
        // the multi-line validator that lands in a later commit.
        let (repl, _server) = spawn_test_repl();
        repl.eval(r#"session.spawn("claude-code", { program = "/bin/sh" })"#)
            .expect("spawn fixture sh");
        // Stash a helper as a global so the lookup-by-pairs assertion can
        // run inside one expression.
        repl.eval(
            r#"function _leaks(t) for k in pairs(t) do if k == "__pty_type" or k == "__pty_adapter" then return true end end return false end"#,
        )
        .expect("define _leaks helper");
        let result = repl
            .eval("_leaks(screen.snapshot())")
            .expect("iterate snapshot keys");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text result, got: {:?}", &result.values[0]);
        };
        assert_eq!(
            text, "false",
            "metatable markers must not appear in pairs()"
        );
        // And confirm the metatable IS readable via getmetatable — that's
        // how the render path detects the tag.
        let result = repl
            .eval(r#"getmetatable(screen.snapshot()).__pty_type"#)
            .expect("read metatable tag");
        let RenderedValue::Text(text) = &result.values[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "screen");
        repl.eval("session.close()").ok();
    }

    #[cfg(unix)]
    #[test]
    fn transcript_snapshot_round_trips_through_rpc() {
        let (repl, _server) = spawn_test_repl();
        repl.eval(r#"session.spawn("claude-code", { program = "/bin/sh" })"#)
            .expect("spawn fixture sh");
        // Default opts (redact = true)
        let result = repl
            .eval("transcript.snapshot()")
            .expect("transcript.snapshot");
        assert_eq!(result.values.len(), 1);
        // And the redact = false escape hatch.
        repl.eval("transcript.snapshot({ redact = false })")
            .expect("transcript.snapshot with redact=false");
        repl.eval("session.close()").ok();
    }

    // ---- multi-line Validator -------------------------------------------

    #[test]
    fn validator_marks_simple_expression_as_complete() {
        let (repl, _server) = spawn_test_repl();
        let validator = LuaValidator::new(repl.lua_handle());
        assert!(matches!(
            reedline::Validator::validate(&validator, "1 + 2"),
            reedline::ValidationResult::Complete
        ));
        assert!(matches!(
            reedline::Validator::validate(&validator, "session.list()"),
            reedline::ValidationResult::Complete
        ));
    }

    #[test]
    fn validator_marks_unclosed_for_block_as_incomplete() {
        let (repl, _server) = spawn_test_repl();
        let validator = LuaValidator::new(repl.lua_handle());
        let outcome = reedline::Validator::validate(
            &validator,
            "for i = 1, 3 do\n  session.spawn(\"claude-code\")",
        );
        assert!(
            matches!(outcome, reedline::ValidationResult::Incomplete),
            "unclosed for-block should be Incomplete"
        );
    }

    #[test]
    fn validator_marks_closed_block_as_complete() {
        let (repl, _server) = spawn_test_repl();
        let validator = LuaValidator::new(repl.lua_handle());
        let outcome = reedline::Validator::validate(
            &validator,
            "for i = 1, 3 do\n  session.spawn(\"x\")\nend",
        );
        assert!(matches!(outcome, reedline::ValidationResult::Complete));
    }

    #[test]
    fn validator_marks_unclosed_table_as_incomplete() {
        let (repl, _server) = spawn_test_repl();
        let validator = LuaValidator::new(repl.lua_handle());
        let outcome = reedline::Validator::validate(&validator, "{ a = 1,");
        assert!(matches!(outcome, reedline::ValidationResult::Incomplete));
    }

    #[test]
    fn validator_marks_meta_command_as_complete() {
        // Meta commands never multi-line — the validator should short-circuit
        // them so reedline doesn't hand them to the Lua parser at all.
        let (repl, _server) = spawn_test_repl();
        let validator = LuaValidator::new(repl.lua_handle());
        assert!(matches!(
            reedline::Validator::validate(&validator, ":focus e1"),
            reedline::ValidationResult::Complete
        ));
    }

    #[test]
    fn validator_marks_permanent_syntax_error_as_complete() {
        // A `++` operator is never valid Lua syntax (no continuation can
        // fix it). The validator must mark it Complete so the eval path
        // surfaces the error instead of leaving the user on a runaway
        // continuation prompt.
        let (repl, _server) = spawn_test_repl();
        let validator = LuaValidator::new(repl.lua_handle());
        assert!(matches!(
            reedline::Validator::validate(&validator, "let me try ++ this"),
            reedline::ValidationResult::Complete
        ));
    }

    #[test]
    fn repl_globals_includes_transcript_screen_inspect() {
        let (repl, _server) = spawn_test_repl();
        for name in ["transcript", "screen", "view", "inspect"] {
            assert!(
                repl.repl_globals.contains(name),
                "expected `{name}` in REPL globals whitelist",
            );
        }
    }

    #[test]
    fn repl_globals_includes_wait_matcher_helpers() {
        let (repl, _server) = spawn_test_repl();
        for name in [
            "wait",
            "matches",
            "screen_stable",
            "cancel_wait",
            "re",
            "ms",
            "s",
        ] {
            assert!(
                repl.repl_globals.contains(name),
                "expected `{name}` in REPL globals whitelist",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn turn_with_matcher_table_round_trips_through_adapter_turn() {
        // End-to-end wire-shape contract for `turn(...)`. The
        // `wait` slot accepts a tagged matcher table built inline; in
        // commit #18 the `matches(...)` constructor returns exactly
        // this shape, so the binding is forward-compatible.
        //
        // The matcher never resolves against /bin/sh output, so the
        // wait leg times out — that's the desired signal here: the
        // call reached the server's adapter.turn → plugin → matcher
        // loop intact. -32001 is the matcher-timeout code defined in
        // src/rpc.rs.
        let (repl, _server) = spawn_test_repl();
        repl.eval(
            r#"session.spawn("claude-code", {
                program = "/bin/sh",
                args = { "-lc", "printf ready; cat" }
            })"#,
        )
        .expect("spawn fixture sh");

        let result = repl.eval(
            r#"turn("send_prompt", {
                prompt = "probe\n",
                wait = { __pty_matcher = "regex", pattern = "never-matches" },
                timeout = 250,
            })"#,
        );
        match result {
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("-32001") || message.contains("matcher"),
                    "expected matcher timeout from adapter.turn, got `{message}`"
                );
            }
            Ok(other) => panic!("expected -32001 matcher timeout, got {other:?}"),
        }
        repl.eval("session.close()").ok();
    }
}
