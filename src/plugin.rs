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
        if self.runtime.is_some()
            && self
                .entrypoint
                .as_deref()
                .is_none_or(|entrypoint| entrypoint.trim().is_empty())
        {
            return Err(PluginManifestError::MissingEntrypoint);
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
            builtin_plugins: vec![claude_code_manifest()],
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
    /// Permission appears more than once.
    #[error("duplicate plugin permission: {0}")]
    DuplicatePermission(String),
}

/// Manifest for the built-in Lua Claude Code adapter.
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
        };

        assert_eq!(
            manifest.validate(),
            Err(PluginManifestError::MissingEntrypoint)
        );
    }

    #[test]
    fn host_capabilities_expose_embedded_lua_and_builtin_claude_plugin() {
        let capabilities = PluginHostCapabilities::current();

        assert!(capabilities.allows(&PluginPermission::SessionSpawn));
        assert!(capabilities.embedded_lua);
        assert!(!capabilities.wasm);
        assert!(capabilities.builtin_plugins.iter().any(
            |plugin| plugin.name == "claude-code" && plugin.runtime == Some(PluginRuntime::Lua)
        ));
    }
}
