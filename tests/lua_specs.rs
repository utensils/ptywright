//! Lua spec runner — drives `tests/lua/spec/*_spec.lua` files through
//! an mlua VM with a tiny busted-compatible DSL (`describe`, `it`,
//! `before_each`, `expect`).
//!
//! Why a custom runner rather than a real busted dep:
//!   * We already vendor Lua 5.4 via mlua's `vendored` feature, so a
//!     standalone Lua interpreter is not in the devshell or required
//!     by source builds. Pulling in `lua5_4` + `luarocks` + `busted`
//!     just to test plugin helpers would inflate the dev surface and
//!     add a moving piece on CI.
//!   * The plugin helpers being tested (`helpers.lua`, `events.lua`)
//!     are pure-Lua modules. The runner here just loads them as Lua
//!     `chunk`s and exposes them as globals — exactly the same
//!     bootstrap [`LuaPlugin::trusted_with_modules`] does, so tests
//!     exercise the modules in the same environment they ship in.
//!   * The DSL is a 100-line wrapper, not a maintained dependency.
//!
//! The runner discovers every `<name>_spec.lua` under
//! `tests/lua/spec/` automatically — drop a new spec file in and it
//! picks up. Each spec runs in its own Lua VM so module-level state
//! (e.g. `events._next_seq`) can't leak between spec files.
//!
//! Each spec file exposes the standard busted API:
//!
//! ```lua
//! describe("module", function()
//!   before_each(function() ... end)
//!   it("does the thing", function()
//!     expect(value).to_equal(other)
//!     expect(value).to_be_truthy()
//!     expect(fn).to_error_with("substring")
//!   end)
//! end)
//! ```
//!
//! Test outcomes propagate to a Rust `Vec<SpecCase>` and fail the
//! `cargo test` invocation on any unsuccessful `it()` block.

use std::path::{Path, PathBuf};

use mlua::{Lua, Value};

#[derive(Debug)]
struct SpecCase {
    file: String,
    describe: String,
    name: String,
    outcome: SpecOutcome,
}

#[derive(Debug)]
enum SpecOutcome {
    Passed,
    Failed { message: String },
}

#[test]
fn run_lua_specs() {
    let spec_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("lua")
        .join("spec");

    let mut spec_files: Vec<PathBuf> = std::fs::read_dir(&spec_dir)
        .unwrap_or_else(|err| panic!("read spec dir {}: {err}", spec_dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension().and_then(|s| s.to_str()) == Some("lua")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| stem.ends_with("_spec"))
        })
        .collect();
    spec_files.sort();

    assert!(
        !spec_files.is_empty(),
        "no <name>_spec.lua files under {}",
        spec_dir.display()
    );

    let mut cases: Vec<SpecCase> = Vec::new();
    for spec_path in &spec_files {
        run_spec_file(spec_path, &mut cases);
    }

    let failed: Vec<&SpecCase> = cases
        .iter()
        .filter(|c| matches!(c.outcome, SpecOutcome::Failed { .. }))
        .collect();

    let passed = cases.len() - failed.len();
    println!(
        "Lua specs: {} passed, {} failed across {} files",
        passed,
        failed.len(),
        spec_files.len()
    );

    if !failed.is_empty() {
        let mut report = String::new();
        for case in &failed {
            let message = match &case.outcome {
                SpecOutcome::Failed { message } => message.as_str(),
                SpecOutcome::Passed => unreachable!(),
            };
            report.push_str(&format!(
                "\n  ✗ {} > {} > {}\n      {}\n",
                case.file, case.describe, case.name, message
            ));
        }
        panic!("{} Lua spec(s) failed:{}", failed.len(), report);
    }
}

fn run_spec_file(path: &Path, cases: &mut Vec<SpecCase>) {
    let lua = new_lua();
    pre_load_plugin_modules(&lua);
    install_spec_dsl(&lua);

    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read spec {}: {err}", path.display()));
    let file_label = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.display().to_string());

    if let Err(err) = lua.load(&source).set_name(&file_label).exec() {
        cases.push(SpecCase {
            file: file_label.clone(),
            describe: "<file load>".into(),
            name: "<unable to evaluate>".into(),
            outcome: SpecOutcome::Failed {
                message: format!("{err}"),
            },
        });
        return;
    }

    let results: Value = lua
        .globals()
        .get("__spec_results__")
        .expect("spec runtime did not register __spec_results__ global");
    let Value::Table(table) = results else {
        panic!("__spec_results__ must be a Lua table; got {results:?}");
    };
    let len = table.raw_len();
    for i in 1..=len {
        let entry: mlua::Table = table
            .get(i)
            .unwrap_or_else(|err| panic!("read spec result {i}: {err}"));
        let describe: String = entry.get("describe").unwrap_or_default();
        let name: String = entry.get("name").unwrap_or_default();
        let passed: bool = entry.get("passed").unwrap_or(false);
        let message: String = entry.get("message").unwrap_or_default();
        cases.push(SpecCase {
            file: file_label.clone(),
            describe,
            name,
            outcome: if passed {
                SpecOutcome::Passed
            } else {
                SpecOutcome::Failed { message }
            },
        });
    }
}

fn new_lua() -> Lua {
    Lua::new_with(
        mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH | mlua::StdLib::UTF8,
        mlua::LuaOptions::new(),
    )
    .expect("init Lua VM")
}

/// Pre-load every claude-code plugin module under the same module
/// names the production runtime uses ([`LuaPlugin::trusted_with_modules`]
/// + [`crate::BUILTIN_PLUGINS`]). Specs reference modules as locals
/// (`local events = events`) just like `main.lua` does.
fn pre_load_plugin_modules(lua: &Lua) {
    let plugins_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("plugins")
        .join("claude-code");
    let modules = [
        ("helpers", plugins_dir.join("helpers.lua")),
        ("events", plugins_dir.join("events.lua")),
    ];
    for (name, path) in &modules {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("read module {}: {err}", path.display()));
        let value: Value = lua
            .load(&source)
            .set_name(*name)
            .eval()
            .unwrap_or_else(|err| panic!("evaluate module {name}: {err}"));
        lua.globals()
            .set(*name, value)
            .unwrap_or_else(|err| panic!("bind module {name}: {err}"));
    }
}

/// Install the busted-flavored DSL: `describe(name, fn)`, `it(name,
/// fn)`, `before_each(fn)`, `expect(value)` returning a matcher table
/// with `.to_equal(other)`, `.to_be_truthy()`, `.to_be_falsy()`,
/// `.to_be_nil()`, `.to_error_with(substring)`. Test outcomes
/// accumulate in `__spec_results__` for the Rust runner to harvest.
fn install_spec_dsl(lua: &Lua) {
    let bootstrap = r#"
        __spec_results__ = {}
        __spec_state__ = { describe_stack = {}, before_each_stack = {} }

        local function describe(name, body)
            table.insert(__spec_state__.describe_stack, name)
            table.insert(__spec_state__.before_each_stack, {})
            local ok, err = pcall(body)
            if not ok then
                table.insert(__spec_results__, {
                    describe = table.concat(__spec_state__.describe_stack, " > "),
                    name = "<describe body>",
                    passed = false,
                    message = "describe body raised: " .. tostring(err),
                })
            end
            table.remove(__spec_state__.describe_stack)
            table.remove(__spec_state__.before_each_stack)
        end

        local function before_each(fn)
            local stack = __spec_state__.before_each_stack
            if #stack == 0 then
                error("before_each called outside describe block")
            end
            table.insert(stack[#stack], fn)
        end

        local function run_before_each()
            for _, hooks in ipairs(__spec_state__.before_each_stack) do
                for _, fn in ipairs(hooks) do fn() end
            end
        end

        local function it(name, body)
            local describe_path = table.concat(__spec_state__.describe_stack, " > ")
            local ok_be, err_be = pcall(run_before_each)
            if not ok_be then
                table.insert(__spec_results__, {
                    describe = describe_path,
                    name = name,
                    passed = false,
                    message = "before_each raised: " .. tostring(err_be),
                })
                return
            end
            local ok, err = pcall(body)
            table.insert(__spec_results__, {
                describe = describe_path,
                name = name,
                passed = ok,
                message = ok and "" or tostring(err),
            })
        end

        local function dump(value)
            if type(value) == "table" then
                local parts = {}
                local seen_array = true
                for k, _ in pairs(value) do
                    if type(k) ~= "number" then seen_array = false; break end
                end
                if seen_array then
                    for _, v in ipairs(value) do
                        table.insert(parts, dump(v))
                    end
                    return "[" .. table.concat(parts, ", ") .. "]"
                end
                for k, v in pairs(value) do
                    table.insert(parts, tostring(k) .. " = " .. dump(v))
                end
                return "{" .. table.concat(parts, ", ") .. "}"
            elseif type(value) == "string" then
                return string.format("%q", value)
            else
                return tostring(value)
            end
        end

        local function deep_equal(a, b)
            if a == b then return true end
            if type(a) ~= type(b) then return false end
            if type(a) ~= "table" then return false end
            for k, v in pairs(a) do
                if not deep_equal(v, b[k]) then return false end
            end
            for k, _ in pairs(b) do
                if a[k] == nil then return false end
            end
            return true
        end

        local function expect(actual)
            local matcher = {}
            function matcher.to_equal(expected)
                if not deep_equal(actual, expected) then
                    error("expected " .. dump(expected) .. ", got " .. dump(actual), 2)
                end
            end
            function matcher.to_be_truthy()
                if not actual then
                    error("expected truthy value, got " .. dump(actual), 2)
                end
            end
            function matcher.to_be_falsy()
                if actual then
                    error("expected falsy value, got " .. dump(actual), 2)
                end
            end
            function matcher.to_be_nil()
                if actual ~= nil then
                    error("expected nil, got " .. dump(actual), 2)
                end
            end
            function matcher.to_error_with(substring)
                if type(actual) ~= "function" then
                    error("to_error_with expects a function argument", 2)
                end
                local ok, err = pcall(actual)
                if ok then
                    error("expected function to error, but it returned successfully", 2)
                end
                if substring and string.find(tostring(err), substring, 1, true) == nil then
                    error("expected error to contain " .. dump(substring) .. ", got " .. tostring(err), 2)
                end
            end
            return matcher
        end

        _G.describe = describe
        _G.it = it
        _G.before_each = before_each
        _G.expect = expect
    "#;
    lua.load(bootstrap)
        .set_name("spec-dsl")
        .exec()
        .expect("install spec DSL");
}
