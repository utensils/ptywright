use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use mlua::LuaSerdeExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::matcher::{PluginRegistry, PredicateContext, PredicateOutcome};
use crate::plugin::{PluginManifest, PluginManifestError, PluginPermission, PluginRuntime};

const LUA_INSTRUCTION_LIMIT: u64 = 5_000_000;
const LUA_WALL_CLOCK_LIMIT: Duration = Duration::from_secs(5);

/// Trusted Lua plugin runtime used for adapter/orchestration code.
///
/// The runtime is intentionally invoked only for explicit adapter calls. PTY byte
/// reading, terminal parsing, screen mutation, and matcher polling remain in Rust.
pub struct LuaPlugin {
    name: String,
    lua: mlua::Lua,
    exports: mlua::RegistryKey,
    permissions: BTreeSet<PluginPermission>,
    instruction_count: Arc<Mutex<u64>>,
    call_started_at: Arc<Mutex<Option<Instant>>>,
    instruction_limit: u64,
    wall_clock_limit: Duration,
}

impl fmt::Debug for LuaPlugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LuaPlugin")
            .field("name", &self.name)
            .field("permissions", &self.permissions)
            .field("instruction_limit", &self.instruction_limit)
            .field("wall_clock_limit", &self.wall_clock_limit)
            .finish_non_exhaustive()
    }
}

impl LuaPlugin {
    /// Create a trusted built-in Lua plugin from embedded source.
    pub fn builtin(name: impl Into<String>, source: &str) -> Result<Self> {
        Self::from_source(
            name.into(),
            source,
            all_host_permissions().into_iter().collect(),
        )
    }

    /// Create a trusted Lua plugin from a validated manifest and source string.
    ///
    /// This is intended for explicit local trusted plugin loading. It is not a
    /// sandbox for untrusted code.
    pub fn trusted(manifest: &PluginManifest, source: &str) -> Result<Self> {
        Self::trusted_with_modules(manifest, source, &[])
    }

    /// Create a trusted Lua plugin with optional auxiliary modules pre-loaded
    /// into the Lua state's globals before the entrypoint runs.
    ///
    /// Each `(name, source)` pair is evaluated in order and the resulting
    /// value is bound to a global named `name`, so the entrypoint can write
    /// `local helpers = helpers` (or use the global directly) without
    /// invoking `require` or touching the filesystem. This lets the
    /// `claude-code` plugin split into focused files (`helpers.lua`,
    /// `indicators.lua`, `parsers.lua`, …) while still shipping as a
    /// single compile-time-embedded built-in.
    pub fn trusted_with_modules(
        manifest: &PluginManifest,
        source: &str,
        modules: &[(&str, &str)],
    ) -> Result<Self> {
        validate_lua_manifest(manifest)?;
        Self::from_source_with_modules(
            manifest.name.clone(),
            source,
            manifest.permissions.iter().cloned().collect(),
            modules,
        )
    }

    /// Load a trusted Lua plugin from a local plugin root and manifest.
    ///
    /// Entrypoints must be relative paths inside the plugin root. Parent
    /// components and absolute paths are rejected to avoid surprising file reads.
    pub fn load_trusted(root: impl AsRef<Path>, manifest: &PluginManifest) -> Result<Self> {
        validate_lua_manifest(manifest)?;
        let entrypoint = manifest.entrypoint.as_deref().ok_or_else(|| {
            Error::Lua(format!(
                "lua plugin `{}` has no entrypoint after validation",
                manifest.name
            ))
        })?;
        let path = trusted_entrypoint_path(root.as_ref(), entrypoint)?;
        let source = fs::read_to_string(&path).map_err(|error| {
            Error::Lua(format!(
                "lua plugin `{}` failed to read entrypoint `{}`: {error}",
                manifest.name,
                path.display()
            ))
        })?;
        Self::trusted(manifest, &source)
    }

    /// Call an exported Lua function with a serializable input value and decode
    /// its return value.
    pub fn call<I, O>(&self, function: &str, input: &I) -> Result<O>
    where
        I: Serialize,
        O: DeserializeOwned,
    {
        *self
            .instruction_count
            .lock()
            .expect("instruction count poisoned") = 0;
        *self.call_started_at.lock().expect("call clock poisoned") = Some(Instant::now());
        let result = (|| {
            let exports: mlua::Table = self.lua.registry_value(&self.exports)?;
            let function: mlua::Function = exports.get(function)?;
            let input = self.lua.to_value(input)?;
            let output: mlua::Value = function.call(input)?;
            self.lua.from_value(output)
        })();
        *self.call_started_at.lock().expect("call clock poisoned") = None;
        result.map_err(|error| self.error(error))
    }

    /// Call an exported Lua function and return raw JSON for tests and generic
    /// future plugin dispatch. Lua numbers are converted through serde JSON's
    /// numeric model; use typed `call` when integer/float distinctions matter.
    pub fn call_value(&self, function: &str, input: &Value) -> Result<Value> {
        self.call(function, input)
    }

    /// Whether this plugin declared a given permission.
    #[must_use]
    pub fn has_permission(&self, permission: &PluginPermission) -> bool {
        self.permissions.contains(permission)
    }

    /// Names of every function exported on the plugin's top-level
    /// returned table.
    ///
    /// Used by `plugin.describe` to enumerate intents and matchers when
    /// the plugin does not provide its own `describe()` catalog. Returns
    /// keys in BTree order so the wire shape is deterministic regardless
    /// of insertion order.
    pub fn exported_function_names(&self) -> Result<Vec<String>> {
        let result = (|| -> mlua::Result<Vec<String>> {
            let exports: mlua::Table = self.lua.registry_value(&self.exports)?;
            let mut names: Vec<String> = Vec::new();
            for pair in exports.pairs::<mlua::Value, mlua::Value>() {
                let (key, value) = pair?;
                if !matches!(value, mlua::Value::Function(_)) {
                    continue;
                }
                if let mlua::Value::String(s) = key {
                    names.push(s.to_str()?.to_owned());
                }
            }
            names.sort();
            Ok(names)
        })();
        result.map_err(|error| self.error(error))
    }

    /// Whether the plugin's exports table has a function with the given
    /// name. Used by `plugin.describe` to decide whether to call
    /// `describe()` vs fall back to introspection.
    ///
    /// Surfaces introspection failures (poisoned registry, table type
    /// changed unexpectedly) through `Error::Lua` rather than swallowing
    /// them as `false` — silently routing a genuine introspection failure
    /// into the fallback path would return an incomplete catalog without
    /// any signal that something went wrong.
    pub fn exports_function(&self, name: &str) -> Result<bool> {
        let result = (|| -> mlua::Result<bool> {
            let exports: mlua::Table = self.lua.registry_value(&self.exports)?;
            let value: mlua::Value = exports.get(name)?;
            Ok(matches!(value, mlua::Value::Function(_)))
        })();
        result.map_err(|error| self.error(error))
    }

    fn from_source(
        name: String,
        source: &str,
        permissions: BTreeSet<PluginPermission>,
    ) -> Result<Self> {
        Self::from_source_with_limits(
            name,
            source,
            permissions,
            &[],
            LUA_INSTRUCTION_LIMIT,
            LUA_WALL_CLOCK_LIMIT,
        )
    }

    fn from_source_with_modules(
        name: String,
        source: &str,
        permissions: BTreeSet<PluginPermission>,
        modules: &[(&str, &str)],
    ) -> Result<Self> {
        Self::from_source_with_limits(
            name,
            source,
            permissions,
            modules,
            LUA_INSTRUCTION_LIMIT,
            LUA_WALL_CLOCK_LIMIT,
        )
    }

    fn from_source_with_limits(
        name: String,
        source: &str,
        permissions: BTreeSet<PluginPermission>,
        modules: &[(&str, &str)],
        instruction_limit: u64,
        wall_clock_limit: Duration,
    ) -> Result<Self> {
        let lua = new_lua().map_err(|error| lua_error(&name, error))?;
        let call_started_at = Arc::new(Mutex::new(Some(Instant::now())));
        let instruction_count = install_execution_limits(
            &lua,
            instruction_limit,
            wall_clock_limit,
            Arc::clone(&call_started_at),
        )
        .map_err(|error| lua_error(&name, error))?;
        install_host_api(&lua, &permissions).map_err(|error| lua_error(&name, error))?;
        *instruction_count
            .lock()
            .expect("instruction count poisoned") = 0;
        *call_started_at.lock().expect("call clock poisoned") = Some(Instant::now());
        // Pre-load auxiliary modules as globals. The module's source is
        // evaluated as a chunk and its return value (typically a table of
        // exported functions) becomes a global named `<module>`. The
        // entrypoint references modules via globals directly:
        //   `local helpers = helpers` (or just `helpers.trim(s)`).
        // No `require` is exposed, so a module can't escape into the
        // filesystem. Errors during module load surface with the
        // module name in their `set_name` context for clear diagnostics.
        for (module_name, module_source) in modules {
            let value_result: mlua::Result<mlua::Value> =
                lua.load(*module_source).set_name(*module_name).eval();
            let value = match value_result {
                Ok(value) => value,
                Err(error) => {
                    *call_started_at.lock().expect("call clock poisoned") = None;
                    return Err(lua_error(&name, error));
                }
            };
            if let Err(error) = lua.globals().set(*module_name, value) {
                *call_started_at.lock().expect("call clock poisoned") = None;
                return Err(lua_error(&name, error));
            }
        }
        let exports_result: mlua::Result<mlua::Table> = lua.load(source).set_name(&name).eval();
        *call_started_at.lock().expect("call clock poisoned") = None;
        let exports = exports_result.map_err(|error| lua_error(&name, error))?;
        let exports = lua
            .create_registry_value(exports)
            .map_err(|error| lua_error(&name, error))?;
        Ok(Self {
            name,
            lua,
            exports,
            permissions,
            instruction_count,
            call_started_at,
            instruction_limit,
            wall_clock_limit,
        })
    }

    fn error(&self, error: mlua::Error) -> Error {
        lua_error(&self.name, error)
    }
}

/// Concrete [`PluginRegistry`] backed by a name → `Arc<Mutex<LuaPlugin>>`
/// map. Threaded into a [`crate::Session`] (via
/// [`crate::Session::set_plugin_registry`]) so the wait loop can
/// evaluate [`crate::Matcher::Lua`] branches against plugin-defined
/// predicates.
///
/// **State separation.** The Lua instances held by this registry are
/// distinct from any [`crate::LuaExtension`]'s own
/// [`LuaPlugin`] — predicates therefore cannot read or mutate
/// module-level state set by a classifier or intent on the
/// adapter-side instance (e.g. claude-code's `_current_dialog_id`).
/// v1 limitation: predicates are stateless w.r.t. classifier state.
/// Predicates that need shared state must carry it through the
/// `params` table on `Matcher::Lua`.
#[derive(Default)]
pub struct LuaPluginRegistry {
    plugins: RwLock<HashMap<String, Arc<Mutex<LuaPlugin>>>>,
}

impl fmt::Debug for LuaPluginRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plugins = self.plugins.read().expect("registry lock poisoned");
        let names: Vec<&String> = plugins.keys().collect();
        formatter
            .debug_struct("LuaPluginRegistry")
            .field("plugins", &names)
            .finish()
    }
}

impl LuaPluginRegistry {
    /// Create an empty registry. Add plugins via [`Self::insert`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience constructor wrapping a single plugin. Most plugin
    /// authors writing tests for a `Matcher::Lua`-emitting plugin only
    /// need the one registry entry.
    #[must_use]
    pub fn with_single(name: impl Into<String>, plugin: LuaPlugin) -> Self {
        let registry = Self::new();
        registry.insert(name, plugin);
        registry
    }

    /// Register a [`LuaPlugin`] under `name`. Replaces any prior entry
    /// with the same name; returns the displaced entry for callers
    /// that want to keep a reference (e.g. graceful unload).
    pub fn insert(
        &self,
        name: impl Into<String>,
        plugin: LuaPlugin,
    ) -> Option<Arc<Mutex<LuaPlugin>>> {
        self.plugins
            .write()
            .expect("registry lock poisoned")
            .insert(name.into(), Arc::new(Mutex::new(plugin)))
    }

    /// Remove the plugin registered under `name`. Returns the
    /// displaced entry if one was present.
    pub fn remove(&self, name: &str) -> Option<Arc<Mutex<LuaPlugin>>> {
        self.plugins
            .write()
            .expect("registry lock poisoned")
            .remove(name)
    }

    /// Whether the registry contains an entry for `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.plugins
            .read()
            .expect("registry lock poisoned")
            .contains_key(name)
    }

    /// Names of every registered plugin, in BTree order so the wire
    /// shape is deterministic.
    #[must_use]
    pub fn plugin_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .plugins
            .read()
            .expect("registry lock poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }
}

impl PluginRegistry for LuaPluginRegistry {
    fn evaluate_predicate(
        &self,
        plugin: &str,
        predicate: &str,
        params: &Value,
        context: &PredicateContext<'_>,
    ) -> Result<PredicateOutcome> {
        let plugin_arc = self
            .plugins
            .read()
            .expect("registry lock poisoned")
            .get(plugin)
            .cloned()
            .ok_or_else(|| {
                Error::Lua(format!(
                    "plugin `{plugin}` not registered in plugin registry"
                ))
            })?;

        // Build the Lua input table: serialize the predicate context to
        // JSON, then merge `params` in under a reserved `params` key.
        // The Lua side sees `input.screen`, `input.markers`,
        // `input.params.foo` etc. Keeping params under its own key
        // avoids collisions with predicate-context field names if the
        // caller's params table contained one of them.
        let mut input = serde_json::to_value(context).map_err(|error| {
            Error::Lua(format!(
                "predicate context serialization for `{plugin}.{predicate}`: {error}"
            ))
        })?;
        if let Value::Object(ref mut obj) = input {
            obj.insert("params".to_string(), params.clone());
        }

        let plugin_guard = plugin_arc.lock().expect("plugin lock poisoned");
        let raw: Value = plugin_guard.call_value(predicate, &input)?;
        parse_predicate_result(plugin, predicate, raw)
    }
}

/// Accept either a boolean shorthand or a structured table from a
/// plugin predicate. Booleans become a [`PredicateOutcome`] with no
/// evidence/capture; tables deserialize directly. Anything else is
/// flagged as a plugin-side bug so authors notice quickly.
fn parse_predicate_result(plugin: &str, predicate: &str, raw: Value) -> Result<PredicateOutcome> {
    match raw {
        Value::Bool(matched) => Ok(PredicateOutcome {
            matched,
            ..PredicateOutcome::default()
        }),
        Value::Null => Ok(PredicateOutcome::default()),
        value @ Value::Object(_) => serde_json::from_value(value).map_err(|error| {
            Error::Lua(format!(
                "predicate `{plugin}.{predicate}` returned an unparseable table: {error}"
            ))
        }),
        other => Err(Error::Lua(format!(
            "predicate `{plugin}.{predicate}` must return bool, nil, or {{matched, evidence?, capture?}} — got {other}"
        ))),
    }
}

fn new_lua() -> mlua::Result<mlua::Lua> {
    mlua::Lua::new_with(
        mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH | mlua::StdLib::UTF8,
        mlua::LuaOptions::new(),
    )
}

fn install_execution_limits(
    _lua: &mlua::Lua,
    _instruction_limit: u64,
    _wall_clock_limit: Duration,
    _call_started_at: Arc<Mutex<Option<Instant>>>,
) -> mlua::Result<Arc<Mutex<u64>>> {
    // Claudette links mlua with the Luau backend. mlua 0.10 does not
    // expose debug hooks for Luau, so the Claudette integration branch
    // cannot install instruction-count or wall-clock hooks here.
    // Adapter calls remain explicit host calls; restore this guard when
    // mlua exposes a Luau-compatible hook/interruption API.
    let instruction_count = Arc::new(Mutex::new(0_u64));
    Ok(instruction_count)
}

fn validate_lua_manifest(manifest: &PluginManifest) -> Result<()> {
    manifest.validate().map_err(lua_manifest_error)?;
    if manifest.runtime != Some(PluginRuntime::Lua) {
        return Err(Error::Lua(format!(
            "plugin `{}` is not a Lua plugin",
            manifest.name
        )));
    }
    Ok(())
}

fn lua_manifest_error(error: PluginManifestError) -> Error {
    Error::Lua(format!("invalid Lua plugin manifest: {error}"))
}

fn trusted_entrypoint_path(root: &Path, entrypoint: &str) -> Result<PathBuf> {
    let entrypoint = Path::new(entrypoint);
    if entrypoint.is_absolute()
        || entrypoint.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(Error::Lua(
            "trusted Lua plugin entrypoint must be a relative path inside the plugin root".into(),
        ));
    }

    let root = fs::canonicalize(root)
        .map_err(|error| Error::Lua(format!("failed to resolve Lua plugin root: {error}")))?;
    let path = fs::canonicalize(root.join(entrypoint)).map_err(|error| {
        Error::Lua(format!(
            "failed to resolve trusted Lua plugin entrypoint: {error}"
        ))
    })?;
    if !path.starts_with(&root) {
        return Err(Error::Lua(
            "trusted Lua plugin entrypoint resolved outside the plugin root".into(),
        ));
    }
    Ok(path)
}

fn lua_error(name: &str, error: mlua::Error) -> Error {
    Error::Lua(format!("lua plugin `{name}` failed: {error}"))
}

fn install_host_api(lua: &mlua::Lua, permissions: &BTreeSet<PluginPermission>) -> mlua::Result<()> {
    let ptywright = lua.create_table()?;
    ptywright.set("action", action_api(lua, permissions)?)?;
    ptywright.set("matcher", matcher_api(lua, permissions)?)?;
    lua.globals().set("ptywright", ptywright)
}

fn action_api(
    lua: &mlua::Lua,
    permissions: &BTreeSet<PluginPermission>,
) -> mlua::Result<mlua::Table> {
    let action = lua.create_table()?;
    if permissions.contains(&PluginPermission::InputWrite) {
        action.set(
            "text",
            lua.create_function(|lua, value: String| tagged_value(lua, "text", value))?,
        )?;
        action.set(
            "paste",
            lua.create_function(|lua, value: String| tagged_value(lua, "paste", value))?,
        )?;
        action.set(
            "bracketed_paste",
            lua.create_function(|lua, value: String| tagged_value(lua, "bracketed_paste", value))?,
        )?;
        action.set(
            "key",
            lua.create_function(|lua, value: String| tagged_value(lua, "key", value))?,
        )?;
        action.set(
            "interrupt",
            lua.create_function(|lua, _: ()| tagged_unit(lua, "interrupt"))?,
        )?;
        action.set(
            "eof",
            lua.create_function(|lua, _: ()| tagged_unit(lua, "eof"))?,
        )?;
        // `mark_transcript` is a metadata annotation, not a PTY write or
        // signal — but it is plugin-initiated state mutation, so it
        // shares `InputWrite` gating with the other actions plugins can
        // emit from their plans. The host applies it via
        // `Session::mark_transcript`; no PTY bytes are written.
        action.set(
            "mark_transcript",
            lua.create_function(|lua, label: String| {
                let value = lua.create_table()?;
                value.set("label", label)?;
                tagged_value(lua, "mark_transcript", value)
            })?,
        )?;
    }
    if permissions.contains(&PluginPermission::SessionKill) {
        action.set(
            "kill",
            lua.create_function(|lua, _: ()| tagged_unit(lua, "kill"))?,
        )?;
        // `signal` needs `SessionKill` rather than `InputWrite` — sending
        // SIGTERM/SIGHUP/SIGUSR* is a lifecycle action, not input. The
        // value is the snake_case Signal variant (see `Signal` in
        // `src/action.rs` for the table).
        action.set(
            "signal",
            lua.create_function(|lua, value: String| tagged_value(lua, "signal", value))?,
        )?;
    }
    Ok(action)
}

fn matcher_api(
    lua: &mlua::Lua,
    permissions: &BTreeSet<PluginPermission>,
) -> mlua::Result<mlua::Table> {
    let matcher = lua.create_table()?;
    if !permissions.contains(&PluginPermission::MatcherWait) {
        return Ok(matcher);
    }
    matcher.set(
        "contains_text",
        lua.create_function(|lua, value: String| tagged_value(lua, "contains_text", value))?,
    )?;
    matcher.set(
        "screen_regex",
        lua.create_function(|lua, value: String| tagged_value(lua, "screen_regex", value))?,
    )?;
    matcher.set(
        "transcript_contains",
        lua.create_function(|lua, value: String| tagged_value(lua, "transcript_contains", value))?,
    )?;
    matcher.set(
        "transcript_regex",
        lua.create_function(|lua, value: String| tagged_value(lua, "transcript_regex", value))?,
    )?;
    matcher.set(
        "screen_stable",
        lua.create_function(|lua, min_ms: u64| {
            let value = lua.create_table()?;
            value.set("min_ms", min_ms)?;
            tagged_value(lua, "screen_stable", value)
        })?,
    )?;
    matcher.set(
        "process_exited",
        lua.create_function(|lua, _: ()| tagged_unit(lua, "process_exited"))?,
    )?;
    matcher.set(
        "any",
        lua.create_function(|lua, value: mlua::Table| tagged_value(lua, "any", value))?,
    )?;
    matcher.set(
        "all",
        lua.create_function(|lua, value: mlua::Table| tagged_value(lua, "all", value))?,
    )?;
    Ok(matcher)
}

fn all_host_permissions() -> [PluginPermission; 7] {
    [
        PluginPermission::SessionSpawn,
        PluginPermission::SessionKill,
        PluginPermission::SessionResize,
        PluginPermission::ScreenRead,
        PluginPermission::TranscriptRead,
        PluginPermission::InputWrite,
        PluginPermission::MatcherWait,
    ]
}

fn tagged_value(
    lua: &mlua::Lua,
    tag: &'static str,
    value: impl mlua::IntoLua,
) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    table.set("type", tag)?;
    table.set("value", value)?;
    Ok(table)
}

fn tagged_unit(lua: &mlua::Lua, tag: &'static str) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    table.set("type", tag)?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::plugin::{PluginKind, PluginPermission};

    use super::*;

    #[test]
    fn trusted_lua_plugin_calls_exported_functions() {
        let plugin = LuaPlugin::builtin(
            "test",
            r#"
            return {
              echo = function(input)
                return { ok = true, message = input.message }
              end
            }
            "#,
        )
        .expect("load lua plugin");

        let value = plugin
            .call_value("echo", &json!({ "message": "hello" }))
            .expect("call lua plugin");

        assert_eq!(value, json!({ "ok": true, "message": "hello" }));
    }

    #[test]
    fn trusted_lua_plugins_receive_action_and_matcher_helpers() {
        let plugin = LuaPlugin::builtin(
            "host-api-test",
            r#"
            return {
              plan = function(_input)
                return {
                  actions = {
                    ptywright.action.text("hello"),
                    ptywright.action.paste("world"),
                    ptywright.action.bracketed_paste("paste me"),
                    ptywright.action.key("enter"),
                    ptywright.action.interrupt(),
                    ptywright.action.eof(),
                    ptywright.action.kill(),
                  },
                  matcher = ptywright.matcher.all({
                    ptywright.matcher.contains_text("ready"),
                    ptywright.matcher.screen_regex("rea.y"),
                    ptywright.matcher.transcript_contains("tail"),
                    ptywright.matcher.transcript_regex("ta.l"),
                    ptywright.matcher.screen_stable(250),
                    ptywright.matcher.process_exited(),
                    ptywright.matcher.any({
                      ptywright.matcher.contains_text("fallback"),
                    }),
                  }),
                }
              end
            }
            "#,
        )
        .expect("load lua plugin");

        let value = plugin
            .call_value("plan", &json!({}))
            .expect("call lua plugin with host helpers");

        assert_eq!(
            value["actions"],
            json!([
                { "type": "text", "value": "hello" },
                { "type": "paste", "value": "world" },
                { "type": "bracketed_paste", "value": "paste me" },
                { "type": "key", "value": "enter" },
                { "type": "interrupt" },
                { "type": "eof" },
                { "type": "kill" }
            ])
        );
        assert_eq!(value["matcher"]["type"], "all");
    }

    #[test]
    fn trusted_manifest_limits_host_helpers_to_declared_permissions() {
        let manifest = PluginManifest {
            name: "limited".to_string(),
            kind: PluginKind::Adapter,
            version: "0.1.0".to_string(),
            runtime: Some(PluginRuntime::Lua),
            entrypoint: Some("main.lua".to_string()),
            permissions: vec![PluginPermission::MatcherWait],
            default_target: None,
        };
        let plugin = LuaPlugin::trusted(
            &manifest,
            r#"
            return {
              has_helpers = function(_input)
                return {
                  action_text = ptywright.action.text ~= nil,
                  matcher_text = ptywright.matcher.contains_text ~= nil,
                }
              end
            }
            "#,
        )
        .expect("load limited plugin");

        assert!(!plugin.has_permission(&PluginPermission::InputWrite));
        assert!(plugin.has_permission(&PluginPermission::MatcherWait));
        let value = plugin
            .call_value("has_helpers", &json!({}))
            .expect("call limited plugin");

        assert_eq!(value["action_text"], false);
        assert_eq!(value["matcher_text"], true);
    }

    #[test]
    fn trusted_manifest_rejects_non_lua_runtime() {
        let manifest = PluginManifest {
            name: "wasm".to_string(),
            kind: PluginKind::Adapter,
            version: "0.1.0".to_string(),
            runtime: Some(PluginRuntime::Wasm),
            entrypoint: Some("main.wasm".to_string()),
            permissions: Vec::new(),
            default_target: None,
        };

        let error = LuaPlugin::trusted(&manifest, "return {}").expect_err("runtime rejected");

        assert!(matches!(error, Error::Lua(_)));
        assert!(error.to_string().contains("not a Lua plugin"));
    }

    #[test]
    fn trusted_entrypoint_must_stay_inside_plugin_root() {
        let manifest = PluginManifest {
            name: "escape".to_string(),
            kind: PluginKind::Adapter,
            version: "0.1.0".to_string(),
            runtime: Some(PluginRuntime::Lua),
            entrypoint: Some("../main.lua".to_string()),
            permissions: Vec::new(),
            default_target: None,
        };

        let error =
            LuaPlugin::load_trusted("plugins/escape", &manifest).expect_err("path rejected");

        assert!(matches!(error, Error::Lua(_)));
        assert!(error.to_string().contains("relative path inside"));
    }

    #[test]
    #[cfg(unix)]
    fn trusted_entrypoint_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("ptywright-lua-plugin-test-{}", std::process::id()));
        let outside = root.with_extension("outside");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&root).expect("create root");
        fs::create_dir_all(&outside).expect("create outside");
        fs::write(outside.join("main.lua"), "return {}").expect("write outside plugin");
        symlink(&outside, root.join("link")).expect("create symlink");

        let manifest = PluginManifest {
            name: "escape".to_string(),
            kind: PluginKind::Adapter,
            version: "0.1.0".to_string(),
            runtime: Some(PluginRuntime::Lua),
            entrypoint: Some("link/main.lua".to_string()),
            permissions: Vec::new(),
            default_target: None,
        };

        let error = LuaPlugin::load_trusted(&root, &manifest).expect_err("symlink rejected");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        assert!(matches!(error, Error::Lua(_)));
        assert!(error.to_string().contains("outside the plugin root"));
    }

    #[test]
    fn lua_plugin_load_and_call_failures_are_lua_errors() {
        let error = LuaPlugin::builtin("bad", "not valid lua").expect_err("load should fail");
        assert!(matches!(error, Error::Lua(_)));

        let plugin = LuaPlugin::builtin("missing", "return {}").expect("load plugin");
        let error = plugin
            .call_value("missing", &json!({}))
            .expect_err("call should fail");
        assert!(matches!(error, Error::Lua(_)));
    }

    #[test]
    fn lua_plugin_calls_have_instruction_limit() {
        let plugin = LuaPlugin::builtin(
            "loop",
            r#"
            return {
              run = function(_input)
                while true do end
              end
            }
            "#,
        )
        .expect("load plugin");

        let error = plugin
            .call_value("run", &json!({}))
            .expect_err("loop should be interrupted");

        assert!(matches!(error, Error::Lua(_)));
        assert!(error.to_string().contains("instruction limit exceeded"));
    }

    #[test]
    fn lua_plugin_registry_evaluates_real_predicate_through_lua() {
        // End-to-end through `LuaPluginRegistry`: load a tiny plugin
        // with a predicate that returns a structured table, evaluate
        // it via the registry, assert the outcome rides back through
        // serde correctly. Verifies the wire shape between Rust's
        // `PredicateOutcome` and Lua's table-or-bool convention.
        let plugin = LuaPlugin::builtin(
            "demo",
            r#"
            return {
              wants_ready = function(input)
                if string.find(input.screen, input.params.anchor) then
                  return { matched = true, evidence = "anchor found", capture = input.params.anchor }
                end
                return false
              end
            }
            "#,
        )
        .expect("load demo plugin");
        let registry = LuaPluginRegistry::with_single("demo", plugin);

        let markers = std::collections::BTreeMap::new();
        let ctx = PredicateContext {
            screen: "session ready",
            transcript: "",
            sequence: 0,
            stable_ms: 0,
            process_exited: false,
            markers: &markers,
            cursor: 0,
        };
        let outcome = registry
            .evaluate_predicate("demo", "wants_ready", &json!({ "anchor": "ready" }), &ctx)
            .expect("evaluate predicate");
        assert!(outcome.matched);
        assert_eq!(outcome.evidence.as_deref(), Some("anchor found"));
        assert_eq!(outcome.capture.as_deref(), Some("ready"));

        // Boolean-shorthand return is accepted too.
        let ctx_no_anchor = PredicateContext {
            screen: "nothing here",
            transcript: "",
            sequence: 0,
            stable_ms: 0,
            process_exited: false,
            markers: &markers,
            cursor: 0,
        };
        let outcome = registry
            .evaluate_predicate(
                "demo",
                "wants_ready",
                &json!({ "anchor": "ready" }),
                &ctx_no_anchor,
            )
            .expect("evaluate predicate (no match)");
        assert!(!outcome.matched);
        assert!(outcome.evidence.is_none());
    }

    #[test]
    fn lua_plugin_registry_errors_on_unknown_plugin() {
        let registry = LuaPluginRegistry::new();
        let markers = std::collections::BTreeMap::new();
        let ctx = PredicateContext {
            screen: "",
            transcript: "",
            sequence: 0,
            stable_ms: 0,
            process_exited: false,
            markers: &markers,
            cursor: 0,
        };
        let err = registry
            .evaluate_predicate("missing", "any", &json!({}), &ctx)
            .expect_err("unknown plugin must error");
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn lua_plugin_registry_rejects_garbage_predicate_return_shape() {
        let plugin = LuaPlugin::builtin(
            "demo",
            r#"
            return {
              wrong_shape = function(_input) return "i am a string, not a predicate" end
            }
            "#,
        )
        .expect("load plugin");
        let registry = LuaPluginRegistry::with_single("demo", plugin);
        let markers = std::collections::BTreeMap::new();
        let ctx = PredicateContext {
            screen: "",
            transcript: "",
            sequence: 0,
            stable_ms: 0,
            process_exited: false,
            markers: &markers,
            cursor: 0,
        };
        let err = registry
            .evaluate_predicate("demo", "wrong_shape", &json!({}), &ctx)
            .expect_err("string return must be rejected");
        assert!(err.to_string().contains("must return bool, nil, or"));
    }

    #[test]
    fn lua_plugin_calls_have_wall_clock_limit() {
        let plugin = LuaPlugin::from_source_with_limits(
            "wall-clock".to_string(),
            r#"
            return {
              run = function(_input)
                while true do end
              end
            }
            "#,
            all_host_permissions().into_iter().collect(),
            &[],
            u64::MAX,
            Duration::from_millis(1),
        )
        .expect("load plugin");

        let error = plugin
            .call_value("run", &json!({}))
            .expect_err("wall-clock loop should be interrupted");

        assert!(matches!(error, Error::Lua(_)));
        assert!(error.to_string().contains("wall-clock limit exceeded"));
    }
}
