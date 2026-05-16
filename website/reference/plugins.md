# Plugins and extensions

ptywright includes a trusted extension surface: declarative manifests, explicit host permissions, JSON-RPC helpers for manifest validation, and an embedded Lua runtime for built-in adapter/orchestration code.

The first embedded plugin is the Claude Code adapter. Rust still owns PTY IO, terminal parsing, screen mutation, actions, matchers, redaction, and RPC framing; Lua is only called for explicit adapter decisions.

## Current model

Implemented:

- `PluginManifest`
- `PluginKind`
- `PluginRuntime`
- `PluginPermission`
- `PluginHostCapabilities`
- trusted embedded Lua execution for built-in plugins
- built-in `claude-code` Lua adapter plugin surfaced via `PluginHostCapabilities.builtin_plugins`
- manifest validation (`PluginManifest::validate`)
- TOML manifest loader for trusted-local third-party plugins (`PluginManifest::load_from_toml_path`)
- CLI: `ptywright serve --plugin <manifest.toml>` (repeatable) for boot-time registration
- JSON-RPC: `plugin.load` / `plugin.unload` for runtime registration when `ptywright serve` is started with `--allow-plugin-load`
- per-method permission gating at the JSON-RPC dispatcher (`-32004 PermissionDenied` with structured `data`)
- JSON-RPC methods:
  - `plugin.capabilities`
  - `plugin.validate_manifest`
  - `plugin.load`
  - `plugin.unload`

Not implemented yet:

- A sandbox for untrusted third-party plugins.
- WASM plugin execution. This has been re-evaluated and intentionally deferred until untrusted marketplace-style plugins become a concrete priority.
- Filesystem, process, or network capabilities for plugin host APIs.
- Per-method permission gating on `session.*` methods (`session.*` operates on directly-created sessions that have no associated plugin manifest; revisit if untrusted plugins become a priority).

## Manifest example

```json
{
  "name": "claude-code",
  "kind": "adapter",
  "version": "0.1.0",
  "runtime": "lua",
  "entrypoint": "plugins/claude-code/main.lua",
  "permissions": ["session.spawn", "screen.read", "input.write", "session.kill"]
}
```

Kinds:

- `adapter`
- `macro`
- `matcher`

Runtimes:

- `lua`
- `wasm` is reserved for future work

Permissions:

- `session.spawn`
- `session.kill`
- `session.resize`
- `screen.read`
- `transcript.read`
- `input.write`
- `matcher.wait`

### Permission gating

Every `adapter.*` JSON-RPC method consults the bound plugin manifest's declared `permissions` before invoking the handler. The mapping is:

| Method                                                       | Required permission |
| ------------------------------------------------------------ | ------------------- |
| `adapter.start`                                              | `session.spawn`     |
| `adapter.send`                                               | `input.write`       |
| `adapter.wait`                                               | `matcher.wait`      |
| `adapter.snapshot` / `adapter.state` / `adapter.inspect`     | `screen.read`       |
| `adapter.transcript`                                         | `transcript.read`   |
| `adapter.close`                                              | `session.kill`      |
| `adapter.list` / `adapter.live`                              | none (read-only)    |

A call against a manifest that lacks the required permission returns `-32004 PermissionDenied` with `data = { "method": "...", "required_permission": "..." }`. Plugin authors should declare the minimal set of permissions their plugin actually uses.

The `plugin.load` / `plugin.unload` JSON-RPC methods are server-mode gated rather than per-plugin gated: they are disabled unless `ptywright serve` was started with `--allow-plugin-load`. When disabled, both return `-32004 PermissionDenied` with `data.reason = "server_did_not_grant_plugin_load"` to distinguish from per-adapter denials.

## Loading third-party plugins

ptywright supports two paths for registering trusted-local third-party plugins beyond the built-in `claude-code` adapter:

1. **At server startup**, pass `--plugin <path/to/manifest.toml>` to `ptywright serve` (repeatable for multiple plugins). The flag works with any transport — `--stdio`, `--socket`, or the default per-user socket. Plugins registered this way are visible through `adapter.list` and instantiable via `adapter.start`.
2. **At runtime**, start the server with `--allow-plugin-load` and call `plugin.load { "manifest_path": "..." }` from a connected client. Symmetric `plugin.unload { "plugin": "<name>" }` deregisters previously loaded plugins (built-ins cannot be unloaded; plugins with live adapters must be closed first).

The manifest's `entrypoint` is resolved relative to the manifest file's parent directory, so a manifest at `/foo/plugins/echo/manifest.toml` with `entrypoint = "main.lua"` loads `/foo/plugins/echo/main.lua`. Absolute paths and `..` traversal components in `entrypoint` are rejected. After string-level validation the resolved entrypoint is canonicalized and asserted to stay inside the canonicalized manifest directory, so a `Normal` path component that turns out to be a symlink pointing outside the plugin's own directory is also rejected.

**Trust model.** Third-party plugins execute in the same trust domain as the built-in claude-code plugin — they run in the same embedded Lua runtime, with the same host helper API, gated by their declared permissions. Only load plugins from trusted local sources. This is not a sandbox for untrusted third-party code.

A worked example (the `echo` fixture from `tests/fixtures/plugins/echo/`):

```toml
# manifest.toml
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

```lua
-- main.lua
local M = {}

function M.classify(_ctx)
  return { state = "ready", confidence = 1.0, evidence = "echo plugin" }
end

function M.send_prompt(input)
  return {
    actions = {
      ptywright.action.text(input.prompt or ""),
      ptywright.action.key("enter"),
    },
    last_intent = "prompt_submitted",
  }
end

function M.wait_turn_matcher(_input)
  return ptywright.matcher.screen_stable(150)
end

return M
```

Load it and drive it:

```bash
ptywright serve --stdio --plugin tests/fixtures/plugins/echo/manifest.toml
```

```json
{ "jsonrpc": "2.0", "id": 1, "method": "adapter.start", "params": { "plugin": "echo" } }
```

## JSON-RPC

Query host capabilities:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "plugin.capabilities" }
```

Validate a manifest:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "plugin.validate_manifest",
  "params": {
    "manifest": {
      "name": "demo",
      "kind": "adapter",
      "version": "0.1.0",
      "runtime": "lua",
      "entrypoint": "main.lua",
      "permissions": ["session.spawn", "screen.read"]
    }
  }
}
```

Validation checks:

- name is non-empty;
- version is non-empty;
- executable runtime declarations include a non-empty entrypoint;
- entrypoints without a runtime are rejected;
- permissions are known;
- permissions are not duplicated.

Unknown permissions fail during JSON deserialization and return `Invalid params`.

## Runtime model

Embedded Lua is available for trusted built-in adapter/orchestration logic. It is not part of the PTY hot path:

- no Lua callbacks run per PTY byte;
- Lua receives explicit screen/transcript snapshots or action inputs;
- Lua uses the injected `ptywright.action.*` and `ptywright.matcher.*` helper APIs to build control plans;
- Lua returns generic `Action`, `Matcher`, and state values;
- Rust installs host helper constructors according to manifest permissions;
- Rust interrupts runaway Lua calls with instruction-count and wall-clock limits;
- No blocking filesystem/process/network host callbacks are exposed to Lua;
- Rust owns session IO and applies redaction at RPC read/error boundaries.

The built-in claude-code plugin lives at `plugins/claude-code/main.lua` and is embedded into the single binary with `include_str!`. Callers drive it through the generic `adapter.*` JSON-RPC surface and `LuaExtension::built_in("claude-code")` in Rust; there is no application-specific RPC namespace or typed Rust adapter shim.

Library callers can load an explicitly trusted local Lua plugin with `LuaPlugin::load_trusted(root, manifest)`. The entrypoint must be a relative path inside the provided plugin root and is resolved after canonicalization to prevent symlink escapes. This is intended for trusted local adapters only; it is not an untrusted plugin sandbox.

WASM remains reserved for a future untrusted plugin model. Until then, prefer out-of-process JSON-RPC clients for untrusted or experimental automation code.
