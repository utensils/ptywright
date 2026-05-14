# Plugins and extensions

ptywright now includes the first extension surface: declarative manifests, explicit host permissions, and JSON-RPC helpers for manifest validation.

There is not yet an embedded plugin runtime. External JSON-RPC clients remain the recommended extension mechanism for early development.

## Current model

Implemented:

- `PluginManifest`
- `PluginKind`
- `PluginPermission`
- `PluginHostCapabilities`
- manifest validation
- JSON-RPC methods:
  - `plugin.capabilities`
  - `plugin.validate_manifest`

Not implemented yet:

- Embedded Lua/Luau execution.
- WASM plugin execution.
- Filesystem, process, or network capabilities for plugins.
- Loading manifests from disk.

## Manifest example

```json
{
  "name": "claude-code",
  "kind": "adapter",
  "version": "0.1.0",
  "permissions": ["session.spawn", "screen.read", "input.write", "session.kill"]
}
```

Kinds:

- `adapter`
- `macro`
- `matcher`

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
      "permissions": ["session.spawn", "screen.read"]
    }
  }
}
```

Validation checks:

- name is non-empty;
- version is non-empty;
- permissions are known;
- permissions are not duplicated.

Unknown permissions fail during JSON deserialization and return `Invalid params`.

## Runtime direction

Phase 1 is external JSON-RPC clients. This is available now and keeps extension code out of the PTY hot path.

Phase 2 may add embedded Lua/Luau for trusted local adapters and macros. Lua must not run per PTY byte or mutate the terminal grid directly.

Phase 3 may add WASM only if untrusted marketplace plugins become important.
