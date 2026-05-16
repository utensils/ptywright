use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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

impl PluginPermission {
    /// String form used in serde, JSON-RPC error data, and log messages. Stays
    /// in sync with the `#[serde(rename = "...")]` attributes above so wire
    /// names and human-readable names cannot drift.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SessionSpawn => "session.spawn",
            Self::SessionKill => "session.kill",
            Self::SessionResize => "session.resize",
            Self::ScreenRead => "screen.read",
            Self::TranscriptRead => "transcript.read",
            Self::InputWrite => "input.write",
            Self::MatcherWait => "matcher.wait",
        }
    }
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
    /// Load a plugin manifest plus its Lua source from a TOML file on disk.
    ///
    /// The manifest's `entrypoint` is resolved relative to the directory
    /// containing the manifest file, so a manifest at
    /// `/foo/plugins/echo/manifest.toml` with `entrypoint = "main.lua"` loads
    /// `/foo/plugins/echo/main.lua`. Absolute `entrypoint` paths and
    /// path-traversing `..` components are rejected to keep the trust model
    /// honest — the host only reads source files inside the plugin's own
    /// directory.
    ///
    /// Returns the validated manifest and the Lua source as a `String`.
    pub fn load_from_toml_path(manifest_path: &Path) -> Result<(Self, String)> {
        let toml_text = std::fs::read_to_string(manifest_path).map_err(|error| {
            Error::Config(format!(
                "failed to read plugin manifest `{}`: {error}",
                manifest_path.display()
            ))
        })?;
        let manifest: Self = toml::from_str(&toml_text).map_err(|error| {
            Error::Config(format!(
                "failed to parse plugin manifest `{}`: {error}",
                manifest_path.display()
            ))
        })?;
        manifest.validate().map_err(|error| {
            Error::Config(format!(
                "invalid plugin manifest `{}`: {error}",
                manifest_path.display()
            ))
        })?;
        let entrypoint = manifest.entrypoint.as_deref().ok_or_else(|| {
            Error::Config(format!(
                "plugin manifest `{}` is missing an entrypoint",
                manifest_path.display()
            ))
        })?;
        let entrypoint_path = Path::new(entrypoint);
        if entrypoint_path.is_absolute()
            || entrypoint_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::Config(format!(
                "plugin manifest `{}` entrypoint `{entrypoint}` must be a relative path inside the manifest's directory",
                manifest_path.display()
            )));
        }
        // Canonicalize the manifest file first so a bare-filename path like
        // `--plugin manifest.toml` (where `Path::parent()` would return
        // `Some("")` and `"".canonicalize()` would fail with `NotFound`)
        // resolves through the same code path as `./plugins/echo/manifest.toml`.
        // The manifest existed when we read it above, so canonicalize cannot
        // fail for legitimate "no such directory" reasons here.
        let manifest_real = manifest_path.canonicalize().map_err(|error| {
            Error::Config(format!(
                "failed to canonicalize plugin manifest `{}`: {error}",
                manifest_path.display()
            ))
        })?;
        let manifest_dir_real = manifest_real.parent().ok_or_else(|| {
            Error::Config(format!(
                "plugin manifest `{}` has no parent directory",
                manifest_path.display()
            ))
        })?;
        let source_path = manifest_dir_real.join(entrypoint_path);
        // Canonicalize the resolved entrypoint and assert it stays inside
        // the manifest's directory. Catches symlink-escape cases the
        // string-level `..` / absolute check above can't see (a
        // `Normal("foo")` component that happens to be a symlink pointing
        // outside the plugin directory).
        let source_real = source_path.canonicalize().map_err(|error| {
            Error::Config(format!(
                "failed to canonicalize plugin entrypoint `{}`: {error}",
                source_path.display()
            ))
        })?;
        if !source_real.starts_with(manifest_dir_real) {
            return Err(Error::Config(format!(
                "plugin manifest `{}` entrypoint `{entrypoint}` escapes the manifest's directory (resolved to `{}`)",
                manifest_path.display(),
                source_real.display()
            )));
        }
        let source = std::fs::read_to_string(&source_real).map_err(|error| {
            Error::Config(format!(
                "failed to read plugin entrypoint `{}`: {error}",
                source_real.display()
            ))
        })?;
        Ok((manifest, source))
    }

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

/// Built-in plugin registry entry: pairs a manifest constructor with the
/// embedded Lua source for that plugin.
///
/// Co-locating the two in a single registry guarantees every registered
/// plugin has a loadable source — there is no defensive "manifest exists
/// but source is missing" branch to maintain.
pub(crate) struct BuiltinPlugin {
    pub manifest: fn() -> PluginManifest,
    pub source: &'static str,
}

/// Every Lua plugin embedded in this binary. Adding a new built-in plugin
/// is a single struct-literal entry here — the manifest constructor and the
/// embedded source travel together so the registry can't be partially
/// populated.
pub(crate) const BUILTIN_PLUGINS: &[BuiltinPlugin] = &[BuiltinPlugin {
    manifest: claude_code_manifest,
    source: include_str!("../plugins/claude-code/main.lua"),
}];

/// Manifests for every Lua plugin embedded in this binary.
///
/// Thin convenience wrapper over [`BUILTIN_PLUGINS`] for callers that only
/// need the manifest metadata (e.g. `adapter.list`,
/// `plugin.capabilities`). To load a plugin by name use
/// [`crate::extension::LuaExtension::built_in`], which consults the same
/// registry and pairs the manifest with the embedded source in one step.
#[must_use]
pub fn builtin_manifests() -> Vec<PluginManifest> {
    BUILTIN_PLUGINS
        .iter()
        .map(|entry| (entry.manifest)())
        .collect()
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

    // ---- load_from_toml_path negative paths ---------------------------

    fn write_temp_manifest(dir: &std::path::Path, manifest_body: &str, lua_body: &str) {
        std::fs::write(dir.join("manifest.toml"), manifest_body).expect("write manifest");
        std::fs::write(dir.join("main.lua"), lua_body).expect("write lua");
    }

    #[test]
    fn load_from_toml_path_rejects_absolute_entrypoint() {
        let dir = tempdir_for_test("plugin-abs-entrypoint");
        write_temp_manifest(
            &dir,
            r#"
name = "abs"
kind = "adapter"
version = "0.1.0"
runtime = "lua"
entrypoint = "/etc/passwd"
permissions = []
"#,
            "-- unused",
        );
        let err = PluginManifest::load_from_toml_path(&dir.join("manifest.toml"))
            .expect_err("absolute entrypoint must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("must be a relative path"),
            "error should explain the rejection: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_toml_path_rejects_parent_traversal() {
        let dir = tempdir_for_test("plugin-parent-traversal");
        write_temp_manifest(
            &dir,
            r#"
name = "escape"
kind = "adapter"
version = "0.1.0"
runtime = "lua"
entrypoint = "../escape.lua"
permissions = []
"#,
            "-- unused",
        );
        let err = PluginManifest::load_from_toml_path(&dir.join("manifest.toml"))
            .expect_err("`..` in entrypoint must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("must be a relative path"),
            "error should explain the rejection: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_toml_path_rejects_missing_entrypoint() {
        // Manifest declares a runtime but no entrypoint — validation should
        // reject before path resolution even runs.
        let dir = tempdir_for_test("plugin-missing-entrypoint");
        std::fs::write(
            dir.join("manifest.toml"),
            r#"
name = "no-entry"
kind = "adapter"
version = "0.1.0"
runtime = "lua"
permissions = []
"#,
        )
        .expect("write manifest");
        let err = PluginManifest::load_from_toml_path(&dir.join("manifest.toml"))
            .expect_err("manifest with runtime but no entrypoint must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("entrypoint") || msg.contains("invalid plugin manifest"),
            "error should mention the missing entrypoint: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_toml_path_accepts_bare_filename_relative_to_cwd() {
        // Regression: `--plugin manifest.toml` (or `plugin.load
        // {"manifest_path": "manifest.toml"}`) from the plugin's directory
        // previously broke because `Path::new("manifest.toml").parent()`
        // returns `Some("")` and `"".canonicalize()` fails with NotFound.
        // The loader now canonicalizes the manifest itself first so the
        // bare-filename case resolves through the same code path as an
        // explicit `./manifest.toml`.
        let dir = tempdir_for_test("plugin-bare-filename");
        write_temp_manifest(
            &dir,
            r#"
name = "bare"
kind = "adapter"
version = "0.1.0"
runtime = "lua"
entrypoint = "main.lua"
permissions = []
"#,
            "return { classify = function() return { state = 'x', confidence = 1.0, evidence = '' } end }",
        );
        // chdir into the manifest's directory so the path passed in is a
        // bare relative filename with no parent component.
        let prev_cwd = std::env::current_dir().expect("save cwd");
        std::env::set_current_dir(&dir).expect("cd into plugin dir");
        let bare = std::path::PathBuf::from("manifest.toml");
        let result = PluginManifest::load_from_toml_path(&bare);
        std::env::set_current_dir(&prev_cwd).expect("restore cwd");
        let (manifest, source) = result.expect("bare relative manifest path must load from cwd");
        assert_eq!(manifest.name, "bare");
        assert!(source.contains("classify"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn load_from_toml_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        // A normal-looking entrypoint that is actually a symlink pointing
        // outside the manifest's directory. String-level checks pass; the
        // canonicalization check should catch it.
        let parent = tempdir_for_test("plugin-symlink-parent");
        let plugin_dir = parent.join("inside");
        let target_dir = parent.join("outside");
        std::fs::create_dir_all(&plugin_dir).expect("create plugin dir");
        std::fs::create_dir_all(&target_dir).expect("create target dir");
        std::fs::write(target_dir.join("escape.lua"), "-- outside").expect("write outside lua");
        // entrypoint points to a symlink whose target is outside the plugin dir.
        symlink(target_dir.join("escape.lua"), plugin_dir.join("main.lua"))
            .expect("create escaping symlink");
        std::fs::write(
            plugin_dir.join("manifest.toml"),
            r#"
name = "symlink-escape"
kind = "adapter"
version = "0.1.0"
runtime = "lua"
entrypoint = "main.lua"
permissions = []
"#,
        )
        .expect("write manifest");
        let err = PluginManifest::load_from_toml_path(&plugin_dir.join("manifest.toml"))
            .expect_err("symlink escaping the plugin directory must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("escapes the manifest's directory"),
            "error should explain the escape: {msg}"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Per-test temp directory under `std::env::temp_dir()`. Returned path is
    /// not auto-cleaned on panic but the negative tests don't write enough
    /// to matter; each test best-effort removes itself.
    fn tempdir_for_test(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ptywright-plugin-test-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }
}
