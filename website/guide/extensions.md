# Extensions

ptywright drives interactive TUIs through a generic extension layer. The `Extension` trait in `src/extension.rs` is the seam between the PTY/screen/action/matcher primitives and any specific TUI's classifier and intents. The Claude Code adapter is the first plugin shipped on top of it; future TUIs land as additional plugins without changes to the core.

## What an extension is

An extension is anything that implements the `Extension` trait. The trait has four methods:

- `manifest() -> &PluginManifest` — return the plugin's stable identifier, version, runtime, and declared permissions. The host uses this for `adapter.list`, `plugin.capabilities`, and permission gating.
- `classify(ctx) -> ExtensionStateSnapshot` — read the current body/status text, transcript tail, last intent, and stability hints from `ClassifyContext`, and return a plugin-defined state string plus confidence and evidence.
- `plan(intent, params) -> ActionPlan` — turn a named intent (for example `"send_prompt"`, `"approve"`, `"cancel"`) into an ordered list of generic `Action` values, optionally recording a `last_intent` for the next classify call.
- `wait_matcher(intent, params) -> Matcher` — build a generic `Matcher` for a named wait intent (for example `"wait_turn_matcher"`).

`ExtensionHandle` in the same module wraps a `Session` plus a boxed `Extension` and drives the host loop: it splits each rendered screen into body and status, calls the classifier, applies action plans, runs intent-supplied wait matchers against the session, and re-classifies after every mutation. The generic core never invents plugin-specific state names or intents; that vocabulary belongs entirely to each plugin.

`LuaExtension` is the only implementor shipped today. It wraps a trusted embedded `LuaPlugin` and exposes its exported functions through the `Extension` trait. A future WASM or external-process plugin would implement the same trait without touching the host loop.

## Plugin manifests

Each plugin declares itself with a `PluginManifest`. Manifests carry a stable name, a category, a version string, the runtime it executes under, a relative entrypoint path, and the host permissions it requests. The built-in Claude Code manifest is a typical adapter manifest:

```json
{
  "name": "claude-code",
  "kind": "adapter",
  "version": "0.1.0",
  "runtime": "lua",
  "entrypoint": "plugins/claude-code/main.lua",
  "permissions": [
    "session.spawn",
    "session.kill",
    "session.resize",
    "screen.read",
    "transcript.read",
    "input.write",
    "matcher.wait"
  ]
}
```

The full list of valid kinds, runtimes, permissions, and manifest fields lives in the [plugins reference](../reference/plugins.md). Validation rejects empty names, empty versions, duplicate permissions, and runtime/entrypoint mismatches.

## Host API exposed to Lua plugins

The trusted Lua runtime in `src/lua_plugin.rs` installs a single global table, `ptywright`, with two helper namespaces. Helpers are gated by the manifest's declared permissions: a plugin that does not request `input.write` will not see the `ptywright.action.*` constructors, and a plugin that does not request `matcher.wait` will not see the `ptywright.matcher.*` constructors.

Action constructors return tagged tables matching the JSON `Action` shape:

- `ptywright.action.text(value)` — typed text input.
- `ptywright.action.paste(value)` — raw paste; writes `value` to the PTY as-is. Use against programs that have NOT enabled bracketed paste (cat, plain shells, generic REPLs).
- `ptywright.action.bracketed_paste(value)` — bracketed paste; writes `value` wrapped in `CSI 200 ~` … `CSI 201 ~`. Use against TUIs that have enabled bracketed paste (Claude Code v2.1+, vim, fish, …) so a subsequent Enter is interpreted as a submit rather than absorbed into the paste tokeniser.
- `ptywright.action.key(name)` — single-key input. `name` is a `snake_case` string matching a variant of the host's `Key` enum. The full surface covers submission/edit keys (`"enter"`, `"escape"`, `"tab"`, `"shift_tab"`, `"backspace"`, `"delete"`, `"space"`), arrows (`"up"`, `"down"`, `"left"`, `"right"`), the navigation cluster (`"home"`, `"end"`, `"page_up"`, `"page_down"`, `"insert"`), every readline-style control combo from `"ctrl_a"` through `"ctrl_z"` except the four that alias other named keys (`ctrl_h`/`ctrl_i`/`ctrl_j`/`ctrl_m` — use `backspace`/`tab`/`enter`), and `"f1"` through `"f12"`. Plugins that accept caller-supplied key strings should normalise hyphens to underscores (`"shift-tab"` → `"shift_tab"`); see the `KEY_ALIASES` table in `plugins/claude-code/main.lua` for the reference implementation.
- `ptywright.action.interrupt()` — Ctrl-C.
- `ptywright.action.eof()` — Ctrl-D.
- `ptywright.action.mark_transcript(label)` — stamp a label-keyed marker at the current transcript cursor without writing any PTY bytes. Plugins emit this from action plans so a single intent dispatch can both write input AND record a turn boundary in one atomic step; the host applies it via `Session::mark_transcript`. Recall the offset later via `Session::transcript_marker(label)` or slice between two markers with `Session::transcript_slice(start, end)`. Requires `input.write` (same gate as the other plan actions).
- `ptywright.action.kill()` — SIGKILL the child; requires the `session.kill` permission.
- `ptywright.action.signal(name)` — deliver an arbitrary process signal; requires the `session.kill` permission. `name` is the snake_case `Signal` variant (`"term"`, `"hup"`, `"quit"`, `"int"`, `"kill"`, `"user1"`, `"user2"`). On Windows only `"term"`, `"int"`, and `"kill"` are honored; other names return `UnsupportedOnPlatform` to the caller.

Matcher constructors return tagged tables matching the JSON `Matcher` shape:

- `ptywright.matcher.contains_text(value)`
- `ptywright.matcher.screen_regex(value)`
- `ptywright.matcher.transcript_contains(value)`
- `ptywright.matcher.transcript_regex(value)`
- `ptywright.matcher.screen_stable(min_ms)`
- `ptywright.matcher.process_exited()`
- `ptywright.matcher.any({ ... })`
- `ptywright.matcher.all({ ... })`

Plugins also receive the `ClassifyContext` fields as a Lua table: `screen`, `body_text`, `status_text`, `transcript`, `sequence`, `last_intent`, `stable_ms`, `completed_turn_stable_ms`, `markers` (the current `label → offset` transcript marker map), and `cursor` (the current `chars_written` count). Body-oriented classifiers should match against `body_text` to avoid false-positives from status-bar substrings; see [Architecture](./architecture.md#abstraction-boundaries) for the body/status split rules. `markers` + `cursor` let a plugin compose transcript metadata (e.g. `metadata.transcript = { turn_start, turn_end }`) in the SAME classify response that requests a `turn_end` mark — see [Plugin-driven transcript markers](../reference/library.md#plugin-driven-transcript-markers-host-marks-markers) for the `host_marks` channel the snapshot can return.

Each plugin call runs under an instruction-count and wall-clock budget; runaway Lua is interrupted before it can block the host loop. The runtime does not expose filesystem, network, or process callbacks to plugins.

## Fixture-driven classifier tests

Recorded screen fixtures live under `plugins/<plugin>/fixtures/` — co-located with the plugin source they exercise so adding a fixture is a single-PR documentation-only change next to the classifier it pins down. Each fixture is a pair:

- `<name>.txt` — sanitized screen capture.
- `<name>.expected.json` — expected state, evidence, optional `last_intent`, and confidence floor.

The Claude Code classifier regression test auto-enrols every fixture under `plugins/claude-code/fixtures/`: dropping in a new `.txt` plus its `.expected.json` is enough for the next `cargo test` run to assert against it. Fixtures without an `.expected.json` sibling are skipped with a warning so exploratory captures can sit alongside graded ones.

When upstream Claude Code changes its TUI (new banner, renamed permission prompt, different plan UI), update the fixture and the classifier in the same PR. The body/status split (with `STATUS_BAR_ROWS = 3`) means status-bar additions usually do not require Lua changes, but body-region changes do.

## Authoring a second adapter

The pieces a second adapter would need are exactly what the Claude Code plugin already demonstrates. Sketch only — none of the code below ships in tree.

1. Write the plugin manifest under `plugins/<name>/manifest.json` (or build it in Rust if the plugin is embedded into the binary like Claude Code is). Request only the permissions the plugin actually uses.

2. Implement the Lua entrypoint with the three exported functions the host loop expects: a `classify(ctx)` table function, one intent function per supported `adapter.send` intent, and one matcher constructor per supported `adapter.wait` intent.

   ```lua
   -- plugins/example/main.lua
   local M = {}
   local action = ptywright.action
   local matcher = ptywright.matcher

   function M.classify(ctx)
     local body = ctx.body_text or ""
     if string.find(body, "Ready>", 1, true) then
       return { state = "ready", confidence = 0.9, evidence = "prompt", sequence = ctx.sequence }
     end
     return { state = "unknown", confidence = 0.0, evidence = "no match", sequence = ctx.sequence }
   end

   function M.send_prompt(input)
     return {
       actions = {
         -- Use `bracketed_paste` when the target TUI has enabled
         -- bracketed paste (DECSET 2004) — that lets the trailing
         -- Enter submit cleanly instead of being absorbed into the
         -- paste tokeniser on longer prompts. Fall back to
         -- `action.paste` when the target is a plain shell / REPL
         -- that has not opted in.
         action.bracketed_paste(input.prompt),
         action.key("enter"),
       },
       last_intent = "prompt_submitted",
     }
   end

   function M.wait_turn_matcher(_input)
     return matcher.all({
       matcher.contains_text("Ready>"),
       matcher.screen_stable(250),
     })
   end

   return M
   ```

3. For an embedded build, add a single `BuiltinPlugin { manifest: foo_manifest, source: include_str!("../plugins/foo/main.lua") }` entry to `BUILTIN_PLUGINS` in `src/plugin.rs`. The manifest constructor and the embedded Lua source travel together, so `LuaExtension::built_in(name)` resolves both in one step — no second registration to keep in sync. Declaring `default_target = { program = "...", args = [...] }` on the manifest lets `adapter.start` callers omit `program`.

4. For an out-of-tree third-party plugin, write the manifest as TOML (next to the Lua source) and register it through one of the trusted-local loading paths below. No Rust changes are needed.

5. Add recorded screen fixtures under `plugins/<name>/fixtures/` (co-located with the plugin source) with sibling `.expected.json` files. If you wire the same auto-enrolling pattern `tests/lua_classifier_tests.rs` uses for claude-code, fixture additions become single-file changes.

There is no per-plugin Rust shim. Application-specific state vocabulary, intent verbs, and evidence strings stay entirely in Lua. Rust callers that want a typed state enum can define one locally and convert from the plugin's state string — that translation is application-specific and intentionally not part of the library surface.

Drivers reach the new plugin through the generic surface: `adapter.start {plugin: "<name>"}`, `adapter.send {adapter, intent, params}`, `adapter.wait {adapter, intent, params, timeout_ms}`. The host loop, body/status split, `last_intent` tracking, and timeout policy are reused unchanged.

## Loading trusted-local third-party plugins

A plugin manifest declared as TOML can be loaded into a `ptywright serve` instance two ways. Both share the same trust model: third-party plugins execute in the same embedded Lua runtime as `claude-code`, with the same host helper surface, gated by their declared permissions. **Only load plugins from trusted local sources** — this is not a sandbox for untrusted code.

### At server startup: `--plugin <manifest.toml>`

Pass the flag to `ptywright serve` (repeatable). The flag works with any transport (`--stdio`, `--socket`, or the default per-user socket). The manifest's `entrypoint` is resolved relative to the manifest file's parent directory; absolute paths, `..` traversal, and symlinks that resolve outside the manifest's own directory are all rejected.

```bash
ptywright serve --socket ~/.ptywright/sockets/default.sock \
  --plugin ~/plugins/echo/manifest.toml \
  --plugin ~/plugins/foo/manifest.toml
```

The plugins are visible through `adapter.list` and instantiable via `adapter.start { "plugin": "echo" }` like any built-in.

### At runtime: `plugin.load` / `plugin.unload`

These JSON-RPC methods register or deregister plugins from a connected client. They are **disabled by default** — start the server with `--allow-plugin-load` to enable them. Without that flag, both methods return `-32004 PermissionDenied` with `data.reason = "server_did_not_grant_plugin_load"` so callers can distinguish a server-mode denial from per-adapter permission denials.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "plugin.load",
  "params": { "manifest_path": "/abs/path/to/manifest.toml" }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "plugin.unload",
  "params": { "plugin": "echo" }
}
```

Built-in plugins (claude-code) cannot be unloaded. Plugins with live adapters bound to them are rejected by `plugin.unload` — `adapter.close` them first.

### Manifest TOML schema

The same fields as the JSON form, with `[default_target]` as a TOML subtable:

```toml
name = "echo"
kind = "adapter"
version = "0.1.0"
runtime = "lua"
entrypoint = "main.lua"
permissions = [
  "session.spawn",
  "session.kill",
  "screen.read",
  "transcript.read",
  "input.write",
  "matcher.wait",
]

[default_target]
program = "/bin/sh"
args = ["-lc", "cat"]
```

Permission names match the wire form documented in [Plugins and extensions](../reference/plugins.md). Declare the minimal subset your plugin actually needs — the JSON-RPC dispatcher's [per-method permission gating](../reference/plugins.md#permission-gating) will reject calls that exceed your declaration with `-32004 PermissionDenied`.
