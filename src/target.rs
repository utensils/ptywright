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
    fn default() -> Self {
        Self::new(24, 80)
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
