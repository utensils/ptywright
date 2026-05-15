use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Extension/plugin category declared by a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// App-specific automation layered above generic sessions.
    Adapter,
    /// Input macro or small action sequence.
    Macro,
    /// Screen/transcript classifier or matcher helper.
    Matcher,
}

/// Trusted plugin runtime declared by a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntime {
    /// Embedded Lua runtime for trusted local adapter/orchestration code.
    Lua,
    /// WebAssembly runtime. Reserved for future untrusted plugin work.
    Wasm,
}

/// Host capability requested by an extension manifest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PluginPermission {
    /// Spawn new PTY sessions.
    #[serde(rename = "session.spawn")]
    SessionSpawn,
    /// Kill or close PTY sessions.
    #[serde(rename = "session.kill")]
    SessionKill,
    /// Resize PTY sessions.
    #[serde(rename = "session.resize")]
    SessionResize,
    /// Read rendered screen snapshots.
    #[serde(rename = "screen.read")]
    ScreenRead,
    /// Read retained transcripts.
    #[serde(rename = "transcript.read")]
    TranscriptRead,
    /// Write input actions to a session.
    #[serde(rename = "input.write")]
    InputWrite,
    /// Wait on matchers.
    #[serde(rename = "matcher.wait")]
    MatcherWait,
}

/// Default PTY target a plugin expects when callers omit the program.
///
/// Plugins declare this in their manifest so the host can wire
/// `adapter.start` to a sensible default without baking application-specific
/// mappings into the RPC layer. Callers may still override by passing
/// `program` / `args` explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultTarget {
    /// PTY program to spawn when the caller does not specify one.
    pub program: String,
    /// Extra CLI arguments appended to the default program.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// Declarative extension manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Stable plugin name.
    pub name: String,
    /// Plugin category.
    pub kind: PluginKind,
    /// Plugin version string.
    pub version: String,
    /// Trusted runtime used by the plugin, if it is executable.
    #[serde(default)]
    pub runtime: Option<PluginRuntime>,
    /// Entrypoint path relative to the plugin root, if it is executable.
    #[serde(default)]
    pub entrypoint: Option<String>,
    /// Requested host permissions.
    #[serde(default)]
    pub permissions: Vec<PluginPermission>,
    /// Default PTY target used by `adapter.start` when the caller omits
    /// `program`. Optional — plugins without a sensible default leave it
    /// `None`, forcing callers to pass `program` explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_target: Option<DefaultTarget>,
}

impl PluginManifest {
    /// Validate manifest fields and duplicate permissions.
    pub fn validate(&self) -> std::result::Result<(), PluginManifestError> {
        if self.name.trim().is_empty() {
            return Err(PluginManifestError::EmptyName);
        }
        if self.version.trim().is_empty() {
            return Err(PluginManifestError::EmptyVersion);
        }
        let has_runtime = self.runtime.is_some();
        let has_entrypoint = self
            .entrypoint
            .as_deref()
            .is_some_and(|entrypoint| !entrypoint.trim().is_empty());
        if has_runtime && !has_entrypoint {
            return Err(PluginManifestError::MissingEntrypoint);
        }
        if !has_runtime && has_entrypoint {
            return Err(PluginManifestError::EntrypointWithoutRuntime);
        }

        let mut seen = BTreeSet::new();
        for permission in &self.permissions {
            if !seen.insert(permission) {
                return Err(PluginManifestError::DuplicatePermission(format!(
                    "{permission:?}"
                )));
            }
        }

        Ok(())
    }
}

/// Static host capability view for plugin planning and validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHostCapabilities {
    /// Permissions this ptywright build understands.
    pub permissions: Vec<PluginPermission>,
    /// Whether embedded Lua/Luau execution is available.
    pub embedded_lua: bool,
    /// Whether WASM plugins are available.
    pub wasm: bool,
    /// Built-in plugins embedded in this single binary.
    pub builtin_plugins: Vec<PluginManifest>,
}

impl PluginHostCapabilities {
    /// Return capabilities for the current build.
    #[must_use]
    pub fn current() -> Self {
        Self {
            permissions: vec![
                PluginPermission::SessionSpawn,
                PluginPermission::SessionKill,
                PluginPermission::SessionResize,
                PluginPermission::ScreenRead,
                PluginPermission::TranscriptRead,
                PluginPermission::InputWrite,
                PluginPermission::MatcherWait,
            ],
            embedded_lua: true,
            wasm: false,
            builtin_plugins: builtin_manifests(),
        }
    }

    /// Check whether a permission is understood by this host.
    #[must_use]
    pub fn allows(&self, permission: &PluginPermission) -> bool {
        self.permissions.contains(permission)
    }
}

/// Manifest validation error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginManifestError {
    /// Plugin name is empty.
    #[error("plugin name must not be empty")]
    EmptyName,
    /// Plugin version is empty.
    #[error("plugin version must not be empty")]
    EmptyVersion,
    /// Executable plugin has no entrypoint.
    #[error("plugin runtime requires an entrypoint")]
    MissingEntrypoint,
    /// Plugin declares an entrypoint but no runtime.
    #[error("plugin entrypoint requires a runtime")]
    EntrypointWithoutRuntime,
    /// Permission appears more than once.
    #[error("duplicate plugin permission: {0}")]
    DuplicatePermission(String),
}

/// Manifests for every Lua plugin embedded in this binary.
///
/// Adding a new built-in plugin is a one-line addition here plus a matching
/// arm in [`builtin_source_for`](crate::extension::builtin_source_for) so the
/// embedded source can be loaded by name. No application-specific Rust code
/// belongs anywhere else in the core.
#[must_use]
pub fn builtin_manifests() -> Vec<PluginManifest> {
    vec![claude_code_manifest()]
}

/// Manifest for the built-in Lua Claude Code adapter.
///
/// One entry in the built-in plugin registry — kept as a named function so
/// the manifest is readable rather than buried inside the registry builder.
#[must_use]
pub fn claude_code_manifest() -> PluginManifest {
    PluginManifest {
        name: "claude-code".to_string(),
        kind: PluginKind::Adapter,
        version: env!("CARGO_PKG_VERSION").to_string(),
        runtime: Some(PluginRuntime::Lua),
        entrypoint: Some("plugins/claude-code/main.lua".to_string()),
        permissions: vec![
            PluginPermission::SessionSpawn,
            PluginPermission::SessionKill,
            PluginPermission::SessionResize,
            PluginPermission::ScreenRead,
            PluginPermission::TranscriptRead,
            PluginPermission::InputWrite,
            PluginPermission::MatcherWait,
        ],
        default_target: Some(DefaultTarget {
            program: "claude".to_string(),
            args: Vec::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn manifest_parses_and_validates_permissions() {
        let manifest: PluginManifest = serde_json::from_value(json!({
            "name": "claude-code",
            "kind": "adapter",
            "version": "0.1.0",
            "runtime": "lua",
            "entrypoint": "main.lua",
            "permissions": ["session.spawn", "screen.read", "input.write"]
        }))
        .expect("parse manifest");

        manifest.validate().expect("valid manifest");
        assert_eq!(manifest.kind, PluginKind::Adapter);
        assert!(manifest.permissions.contains(&PluginPermission::ScreenRead));
    }

    #[test]
    fn manifest_rejects_empty_name() {
        let manifest = PluginManifest {
            name: " ".to_string(),
            kind: PluginKind::Adapter,
            version: "0.1.0".to_string(),
            runtime: None,
            entrypoint: None,
            permissions: Vec::new(),
            default_target: None,
        };

        assert_eq!(manifest.validate(), Err(PluginManifestError::EmptyName));
    }

    #[test]
    fn manifest_rejects_duplicate_permissions() {
        let manifest = PluginManifest {
            name: "dup".to_string(),
            kind: PluginKind::Macro,
            version: "0.1.0".to_string(),
            runtime: None,
            entrypoint: None,
            permissions: vec![PluginPermission::InputWrite, PluginPermission::InputWrite],
            default_target: None,
        };

        assert!(matches!(
            manifest.validate(),
            Err(PluginManifestError::DuplicatePermission(_))
        ));
    }

    #[test]
    fn executable_manifest_requires_entrypoint() {
        let manifest = PluginManifest {
            name: "missing-entrypoint".to_string(),
            kind: PluginKind::Adapter,
            version: "0.1.0".to_string(),
            runtime: Some(PluginRuntime::Lua),
            entrypoint: None,
            permissions: Vec::new(),
            default_target: None,
        };

        assert_eq!(
            manifest.validate(),
            Err(PluginManifestError::MissingEntrypoint)
        );
    }

    #[test]
    fn manifest_rejects_entrypoint_without_runtime() {
        let manifest = PluginManifest {
            name: "missing-runtime".to_string(),
            kind: PluginKind::Adapter,
            version: "0.1.0".to_string(),
            runtime: None,
            entrypoint: Some("main.lua".to_string()),
            permissions: Vec::new(),
            default_target: None,
        };

        assert_eq!(
            manifest.validate(),
            Err(PluginManifestError::EntrypointWithoutRuntime)
        );
    }

    #[test]
    fn host_capabilities_expose_embedded_lua_and_builtin_plugins() {
        let capabilities = PluginHostCapabilities::current();

        assert!(capabilities.allows(&PluginPermission::SessionSpawn));
        assert!(capabilities.embedded_lua);
        assert!(!capabilities.wasm);
        // The registry must include claude-code (the only built-in today) and
        // surface its default_target so adapter.start can spawn without an
        // explicit program. New built-ins should add a similar assertion.
        let claude = capabilities
            .builtin_plugins
            .iter()
            .find(|plugin| plugin.name == "claude-code")
            .expect("claude-code is registered as a built-in plugin");
        assert_eq!(claude.runtime, Some(PluginRuntime::Lua));
        let default_target = claude
            .default_target
            .as_ref()
            .expect("claude-code declares a default spawn target");
        assert_eq!(default_target.program, "claude");
        // Locking in "interactive only": the default args must stay empty
        // so a future edit can't silently add `-p` / `--print` and break
        // every caller that relies on the interactive TUI.
        assert!(
            default_target.args.is_empty(),
            "claude-code default_target.args must stay empty (no `-p`/`--print`); got {:?}",
            default_target.args,
        );
    }

    #[test]
    fn manifest_default_target_round_trips_through_serde() {
        let manifest: PluginManifest = serde_json::from_value(json!({
            "name": "demo",
            "kind": "adapter",
            "version": "0.1.0",
            "default_target": { "program": "demo-bin", "args": ["--interactive"] }
        }))
        .expect("parse manifest with default_target");
        let target = manifest
            .default_target
            .as_ref()
            .expect("default_target present");
        assert_eq!(target.program, "demo-bin");
        assert_eq!(target.args, vec!["--interactive".to_string()]);

        // Round-trip back to JSON; the empty-args variant should be omitted
        // so manifests stay tidy when callers don't need extra arguments.
        let manifest = PluginManifest {
            name: "demo".to_string(),
            kind: PluginKind::Adapter,
            version: "0.1.0".to_string(),
            runtime: None,
            entrypoint: None,
            permissions: Vec::new(),
            default_target: Some(DefaultTarget {
                program: "x".to_string(),
                args: Vec::new(),
            }),
        };
        let value = serde_json::to_value(&manifest).expect("serialize manifest");
        let target = value
            .get("default_target")
            .and_then(|v| v.as_object())
            .expect("default_target serialised");
        assert_eq!(target.get("program"), Some(&json!("x")));
        assert!(target.get("args").is_none(), "empty args should be skipped");
    }
}
