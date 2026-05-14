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
- built-in `claude-code` Lua adapter plugin
- manifest validation
- JSON-RPC methods:
  - `plugin.capabilities`
  - `plugin.validate_manifest`

Not implemented yet:

- Loading third-party Lua plugins from disk.
- WASM plugin execution.
- Filesystem, process, or network capabilities for plugins.

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
- permissions are known;
- permissions are not duplicated.

Unknown permissions fail during JSON deserialization and return `Invalid params`.

## Runtime model

Embedded Lua is available for trusted built-in adapter/orchestration logic. It is not part of the PTY hot path:

- no Lua callbacks run per PTY byte;
- Lua receives explicit screen/transcript snapshots or action inputs;
- Lua uses the injected `ptywright.action.*` and `ptywright.matcher.*` helper APIs to build control plans;
- Lua returns generic `Action`, `Matcher`, and state values;
- Rust enforces permissions, owns session IO, and applies redaction at RPC read boundaries.

The built-in Claude Code plugin lives at `plugins/claude-code/main.lua` and is embedded into the single binary with `include_str!`. The public `claude.*` methods are compatibility wrappers over that Lua adapter.

WASM remains reserved for a future untrusted plugin model.
