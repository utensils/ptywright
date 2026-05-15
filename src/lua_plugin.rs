use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::LuaSerdeExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::plugin::{PluginManifest, PluginManifestError, PluginPermission, PluginRuntime};

const LUA_INSTRUCTION_HOOK_INTERVAL: u32 = 10_000;
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
    instruction_count: Rc<Cell<u64>>,
    call_started_at: Rc<Cell<Option<Instant>>>,
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
        validate_lua_manifest(manifest)?;
        Self::from_source(
            manifest.name.clone(),
            source,
            manifest.permissions.iter().cloned().collect(),
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
        self.instruction_count.set(0);
        self.call_started_at.set(Some(Instant::now()));
        let result = (|| {
            let exports: mlua::Table = self.lua.registry_value(&self.exports)?;
            let function: mlua::Function = exports.get(function)?;
            let input = self.lua.to_value(input)?;
            let output: mlua::Value = function.call(input)?;
            self.lua.from_value(output)
        })();
        self.call_started_at.set(None);
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

    fn from_source(
        name: String,
        source: &str,
        permissions: BTreeSet<PluginPermission>,
    ) -> Result<Self> {
        Self::from_source_with_limits(
            name,
            source,
            permissions,
            LUA_INSTRUCTION_LIMIT,
            LUA_WALL_CLOCK_LIMIT,
        )
    }

    fn from_source_with_limits(
        name: String,
        source: &str,
        permissions: BTreeSet<PluginPermission>,
        instruction_limit: u64,
        wall_clock_limit: Duration,
    ) -> Result<Self> {
        let lua = new_lua().map_err(|error| lua_error(&name, error))?;
        let call_started_at = Rc::new(Cell::new(Some(Instant::now())));
        let instruction_count = install_execution_limits(
            &lua,
            instruction_limit,
            wall_clock_limit,
            Rc::clone(&call_started_at),
        )
        .map_err(|error| lua_error(&name, error))?;
        install_host_api(&lua, &permissions).map_err(|error| lua_error(&name, error))?;
        instruction_count.set(0);
        call_started_at.set(Some(Instant::now()));
        let exports_result: mlua::Result<mlua::Table> = lua.load(source).set_name(&name).eval();
        call_started_at.set(None);
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

fn new_lua() -> mlua::Result<mlua::Lua> {
    mlua::Lua::new_with(
        mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH | mlua::StdLib::UTF8,
        mlua::LuaOptions::new(),
    )
}

fn install_execution_limits(
    lua: &mlua::Lua,
    instruction_limit: u64,
    wall_clock_limit: Duration,
    call_started_at: Rc<Cell<Option<Instant>>>,
) -> mlua::Result<Rc<Cell<u64>>> {
    let instruction_count = Rc::new(Cell::new(0_u64));
    let hook_count = Rc::clone(&instruction_count);
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(LUA_INSTRUCTION_HOOK_INTERVAL),
        move |_lua, _debug| {
            let next = hook_count
                .get()
                .saturating_add(u64::from(LUA_INSTRUCTION_HOOK_INTERVAL));
            hook_count.set(next);
            if next > instruction_limit {
                return Err(mlua::Error::RuntimeError(
                    "Lua plugin instruction limit exceeded".to_string(),
                ));
            }
            if let Some(started_at) = call_started_at.get()
                && started_at.elapsed() > wall_clock_limit
            {
                return Err(mlua::Error::RuntimeError(
                    "Lua plugin wall-clock limit exceeded".to_string(),
                ));
            }
            Ok(mlua::VmState::Continue)
        },
    )?;
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
    }
    if permissions.contains(&PluginPermission::SessionKill) {
        action.set(
            "kill",
            lua.create_function(|lua, _: ()| tagged_unit(lua, "kill"))?,
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
