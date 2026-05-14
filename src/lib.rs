//! Core library surface for `ptywright`.
//!
//! The crate is intentionally small today: it establishes the package, CLI,
//! documentation, release, and Nix plumbing for a future PTY/TUI automation
//! toolkit. The long-term shape is a cross-platform, general-purpose driver
//! that can control interactive terminal applications without being coupled to
//! any one TUI.

/// Crate and binary name.
pub const NAME: &str = "ptywright";

/// Package version from Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short package description from Cargo metadata.
pub const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

/// A terminal program target that a future driver can spawn or attach to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Executable name or path.
    pub program: String,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
}

impl Target {
    /// Create a target with no arguments.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Add one argument and return the updated target.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_builder_collects_program_and_args() {
        let target = Target::new("python").arg("-i");

        assert_eq!(target.program, "python");
        assert_eq!(target.args, ["-i"]);
    }
}
