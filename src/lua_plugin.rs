use std::fmt;

use mlua::LuaSerdeExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::{Error, Result};

/// Trusted Lua plugin runtime used for adapter/orchestration code.
///
/// The runtime is intentionally invoked only for explicit adapter calls. PTY byte
/// reading, terminal parsing, screen mutation, and matcher polling remain in Rust.
pub struct LuaPlugin {
    name: &'static str,
    lua: mlua::Lua,
    exports: mlua::RegistryKey,
}

impl fmt::Debug for LuaPlugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LuaPlugin")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl LuaPlugin {
    /// Create a trusted built-in Lua plugin from embedded source.
    pub fn builtin(name: &'static str, source: &'static str) -> Result<Self> {
        let lua = mlua::Lua::new();
        install_host_api(&lua).map_err(|error| lua_error(name, error))?;
        let exports: mlua::Table = lua
            .load(source)
            .set_name(name)
            .eval()
            .map_err(|error| lua_error(name, error))?;
        let exports = lua
            .create_registry_value(exports)
            .map_err(|error| lua_error(name, error))?;
        Ok(Self { name, lua, exports })
    }

    /// Call an exported Lua function with a serializable input value and decode
    /// its return value.
    pub fn call<I, O>(&self, function: &str, input: &I) -> Result<O>
    where
        I: Serialize,
        O: DeserializeOwned,
    {
        let exports: mlua::Table = self
            .lua
            .registry_value(&self.exports)
            .map_err(|error| self.error(error))?;
        let function: mlua::Function = exports.get(function).map_err(|error| self.error(error))?;
        let input = self
            .lua
            .to_value(input)
            .map_err(|error| self.error(error))?;
        let output: mlua::Value = function.call(input).map_err(|error| self.error(error))?;
        self.lua
            .from_value(output)
            .map_err(|error| self.error(error))
    }

    /// Call an exported Lua function and return raw JSON for tests and generic
    /// future plugin dispatch. Lua numbers are converted through serde JSON's
    /// numeric model; use typed `call` when integer/float distinctions matter.
    pub fn call_value(&self, function: &str, input: &Value) -> Result<Value> {
        self.call(function, input)
    }

    fn error(&self, error: mlua::Error) -> Error {
        lua_error(self.name, error)
    }
}

fn lua_error(name: &str, error: mlua::Error) -> Error {
    Error::Lua(format!("lua plugin `{name}` failed: {error}"))
}

fn install_host_api(lua: &mlua::Lua) -> mlua::Result<()> {
    let ptywright = lua.create_table()?;
    ptywright.set("action", action_api(lua)?)?;
    ptywright.set("matcher", matcher_api(lua)?)?;
    lua.globals().set("ptywright", ptywright)
}

fn action_api(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    let action = lua.create_table()?;
    action.set(
        "text",
        lua.create_function(|lua, value: String| tagged_value(lua, "text", value))?,
    )?;
    action.set(
        "paste",
        lua.create_function(|lua, value: String| tagged_value(lua, "paste", value))?,
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
    action.set(
        "kill",
        lua.create_function(|lua, _: ()| tagged_unit(lua, "kill"))?,
    )?;
    Ok(action)
}

fn matcher_api(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    let matcher = lua.create_table()?;
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
                { "type": "key", "value": "enter" },
                { "type": "interrupt" },
                { "type": "eof" },
                { "type": "kill" }
            ])
        );
        assert_eq!(value["matcher"]["type"], "all");
    }

    #[test]
    fn lua_plugin_load_and_call_failures_are_lua_errors() {
        let error = LuaPlugin::builtin("bad", "not valid lua").expect_err("load should fail");
        assert!(matches!(error, Error::Lua(_)));

        let plugin = LuaPlugin::builtin("missing", "return {}").expect("load plugin");
        let error = plugin
            .call_value("missing", &json!({}))
            .expect_err("missing function should fail");
        assert!(matches!(error, Error::Lua(_)));
    }
}
