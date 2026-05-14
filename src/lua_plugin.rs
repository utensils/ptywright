use mlua::LuaSerdeExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::{Error, Result};

/// Trusted Lua plugin runtime used for adapter/orchestration code.
///
/// The runtime is intentionally invoked only for explicit adapter calls. PTY byte
/// reading, terminal parsing, screen mutation, and matcher polling remain in Rust.
#[derive(Debug, Clone, Copy)]
pub struct LuaPlugin {
    name: &'static str,
    source: &'static str,
}

impl LuaPlugin {
    /// Create a trusted built-in Lua plugin from embedded source.
    #[must_use]
    pub const fn builtin(name: &'static str, source: &'static str) -> Self {
        Self { name, source }
    }

    /// Call an exported Lua function with a serializable input value and decode
    /// its return value.
    pub fn call<I, O>(&self, function: &str, input: &I) -> Result<O>
    where
        I: Serialize,
        O: DeserializeOwned,
    {
        let lua = mlua::Lua::new();
        install_host_api(&lua).map_err(|error| self.error(error))?;
        let exports: mlua::Table = lua
            .load(self.source)
            .set_name(self.name)
            .eval()
            .map_err(|error| self.error(error))?;
        let function: mlua::Function = exports.get(function).map_err(|error| self.error(error))?;
        let input = lua.to_value(input).map_err(|error| self.error(error))?;
        let output: mlua::Value = function.call(input).map_err(|error| self.error(error))?;
        lua.from_value(output).map_err(|error| self.error(error))
    }

    /// Call an exported Lua function and return raw JSON for tests and generic
    /// future plugin dispatch.
    pub fn call_value(&self, function: &str, input: &Value) -> Result<Value> {
        self.call(function, input)
    }

    fn error(&self, error: mlua::Error) -> Error {
        Error::Rpc(format!("lua plugin `{}` failed: {error}", self.name))
    }
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
        );

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
                  action = ptywright.action.key("enter"),
                  matcher = ptywright.matcher.contains_text("ready"),
                }
              end
            }
            "#,
        );

        let value = plugin
            .call_value("plan", &json!({}))
            .expect("call lua plugin with host helpers");

        assert_eq!(
            value,
            json!({
                "action": { "type": "key", "value": "enter" },
                "matcher": { "type": "contains_text", "value": "ready" }
            })
        );
    }
}
