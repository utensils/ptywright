use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Size of the terminal visible area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    /// Number of terminal rows.
    pub rows: u16,
    /// Number of terminal columns.
    pub cols: u16,
    /// Cell-area width in pixels when known.
    pub pixel_width: u16,
    /// Cell-area height in pixels when known.
    pub pixel_height: u16,
}

impl TerminalSize {
    /// Create a terminal size from rows and columns.
    #[must_use]
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Default for TerminalSize {
    /// Default size (50 rows × 120 cols) is a modest bump over the
    /// classic 24×80 — generous enough that typical tool-call rows
    /// and file paths don't wrap mid-string, but still in
    /// "normal-terminal" territory so consumer TUIs aren't surprised
    /// by an unusual viewport. An earlier attempt at 200×200 broke
    /// Claude Code's startup input handling (welcome-banner redraw
    /// swallowed bracketed-paste sequences).
    ///
    /// Consumers driving large agent output (e.g. Claudette's Claude
    /// Code harness) should pass an explicit `TerminalSize` sized to
    /// their workload rather than relying on this default — the
    /// default is intentionally conservative so generic ptywright
    /// callers get safe behaviour.
    fn default() -> Self {
        Self::new(50, 120)
    }
}

/// A terminal program target that ptywright can spawn in a PTY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// Executable name or path.
    pub program: String,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
    /// Optional working directory.
    pub cwd: Option<PathBuf>,
    /// Environment overrides for the child process.
    pub env: BTreeMap<String, String>,
    /// Initial terminal size.
    pub size: TerminalSize,
    /// When `true`, the child is spawned with an empty environment
    /// except for [`Target::env`] entries (plus the manifest's required
    /// env when the target is built from a plugin manifest). Defaults to
    /// `false` — the parent process env is inherited and `env` overlays
    /// on top, matching `std::process::Command` behaviour.
    ///
    /// Set via [`Target::clear_env`] when consuming a plugin manifest
    /// whose `default_target.required_env` declares safety-critical
    /// settings that must not leak from the parent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub clear_env: bool,
}

const fn is_false(b: &bool) -> bool {
    !*b
}

impl Target {
    /// Create a target with no arguments and a default 24x80 terminal.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            size: TerminalSize::default(),
            clear_env: false,
        }
    }

    /// Add one argument and return the updated target.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add multiple arguments and return the updated target.
    #[must_use]
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set one environment override.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set the initial terminal size.
    #[must_use]
    pub fn size(mut self, size: TerminalSize) -> Self {
        self.size = size;
        self
    }

    /// Spawn the child with an empty environment, ignoring the parent
    /// process env. [`Target::env`] entries are still applied; combined
    /// with a manifest's `default_target.required_env`, this is how
    /// callers opt into a fully reproducible env layout (e.g. when the
    /// plugin's behaviour depends on a precise env layout and parent
    /// env should not leak).
    #[must_use]
    pub fn clear_env(mut self) -> Self {
        self.clear_env = true;
        self
    }

    /// Borrow the current env map. Useful for env-drift detection — pair
    /// with a later resolution to spot keys added or changed between
    /// spawns. Matches the field that ends up applied to the child's
    /// environment after merging with the plugin manifest.
    #[must_use]
    pub fn env_snapshot(&self) -> &BTreeMap<String, String> {
        &self.env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_env_builder_sets_flag_and_env_snapshot_returns_overlay() {
        // `clear_env` is a builder bool that the spawn path consults to
        // call `command.env_clear()` before applying the overlay. The
        // overlay itself is still visible through `env_snapshot()` so
        // callers can compare it against a later resolution for
        // env-drift detection.
        let target = Target::new("demo")
            .env("FOO", "bar")
            .env("BAZ", "qux")
            .clear_env();
        assert!(target.clear_env);
        let snapshot = target.env_snapshot();
        assert_eq!(snapshot.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(snapshot.get("BAZ").map(String::as_str), Some("qux"));
        assert!(!snapshot.contains_key("PATH"));
    }

    #[test]
    fn target_builder_collects_program_args_cwd_env_and_size() {
        let target = Target::new("python")
            .arg("-i")
            .args(["-q"])
            .cwd("/tmp")
            .env("TERM", "xterm-256color")
            .size(TerminalSize::new(40, 120));

        assert_eq!(target.program, "python");
        assert_eq!(target.args, ["-i", "-q"]);
        assert_eq!(target.cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(
            target.env.get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_eq!(target.size, TerminalSize::new(40, 120));
    }
}
