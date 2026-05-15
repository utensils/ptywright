//! Resolution and layout for the per-user `~/.ptywright/` runtime directory.
//!
//! ptywright keeps configuration, logs, and (future) cached state under one
//! root rather than splitting across XDG locations. Resolution order:
//!
//! 1. `PTYWRIGHT_HOME` environment variable.
//! 2. `~/.ptywright/` (via [`dirs::home_dir`]).
//! 3. `./.ptywright` as a last resort if `HOME` is unset.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Environment variable that overrides the runtime directory location.
pub const ENV_HOME: &str = "PTYWRIGHT_HOME";

/// Default directory name under the user's home directory.
pub const DEFAULT_DIR_NAME: &str = ".ptywright";

/// Per-user runtime layout for ptywright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Build a [`Paths`] rooted at an explicit directory. Useful for tests and
    /// callers that want to bypass environment lookup.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve the runtime directory using the standard precedence:
    /// `PTYWRIGHT_HOME` env var → `~/.ptywright/` → `./.ptywright`.
    #[must_use]
    pub fn from_env() -> Self {
        if let Some(value) = std::env::var_os(ENV_HOME) {
            let value = PathBuf::from(value);
            if !value.as_os_str().is_empty() {
                return Self::with_root(value);
            }
        }
        let root = dirs::home_dir()
            .map(|home| home.join(DEFAULT_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR_NAME));
        Self::with_root(root)
    }

    /// Root directory of the per-user runtime tree.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.root
    }

    /// Path to the optional TOML config file.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// Directory for rotated log files.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Directory for arbitrary on-disk state owned by ptywright.
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Directory for raw transcript files when explicit per-session opt-in is enabled.
    #[must_use]
    pub fn transcripts_dir(&self) -> PathBuf {
        self.root.join("transcripts")
    }

    /// Directory for IPC sockets/named-pipe paths created by `serve --socket`.
    #[must_use]
    pub fn sockets_dir(&self) -> PathBuf {
        self.root.join("sockets")
    }

    /// Create `dir` (and parents) on demand, returning the path back for chaining.
    /// Performs no work when the directory already exists.
    pub fn ensure_dir(dir: impl AsRef<Path>) -> Result<PathBuf> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(Error::from)?;
        Ok(dir.to_path_buf())
    }

    /// Convenience: ensure [`Self::logs_dir`] exists.
    pub fn ensure_logs_dir(&self) -> Result<PathBuf> {
        Self::ensure_dir(self.logs_dir())
    }
}

/// Expand a leading `~` in `value` to the current user's home directory.
///
/// - `"~"` → `home_dir()`
/// - `"~/foo"` → `home_dir().join("foo")`
/// - everything else returned verbatim.
///
/// When `HOME` is unset, the value is returned unchanged. This mirrors mold's
/// tilde-expansion helper and keeps behavior predictable in unit tests.
#[must_use]
pub fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(value));
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tempdir(label: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("ptywright-{label}-{}-{suffix}", std::process::id()))
    }

    #[test]
    fn with_root_uses_explicit_path_without_touching_env() {
        let root = PathBuf::from("/tmp/explicit");
        let paths = Paths::with_root(&root);

        assert_eq!(paths.home(), root);
        assert_eq!(paths.config_path(), root.join("config.toml"));
        assert_eq!(paths.logs_dir(), root.join("logs"));
        assert_eq!(paths.data_dir(), root.join("data"));
        assert_eq!(paths.transcripts_dir(), root.join("transcripts"));
        assert_eq!(paths.sockets_dir(), root.join("sockets"));
    }

    #[test]
    fn ensure_dir_creates_parents_and_is_idempotent() {
        let root = unique_tempdir("ensure-dir");
        let target = root.join("nested").join("deeper");

        let returned = Paths::ensure_dir(&target).expect("create nested dir");
        assert_eq!(returned, target);
        assert!(target.exists(), "directory should be created");

        // Calling twice must not error.
        Paths::ensure_dir(&target).expect("ensure existing dir");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ensure_logs_dir_resolves_under_root() {
        let root = unique_tempdir("logs-dir");
        let paths = Paths::with_root(&root);

        let logs = paths.ensure_logs_dir().expect("create logs dir");
        assert_eq!(logs, root.join("logs"));
        assert!(logs.exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn expand_tilde_passes_through_non_tilde_paths() {
        assert_eq!(expand_tilde("/tmp/abs"), PathBuf::from("/tmp/abs"));
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
        assert_eq!(expand_tilde(""), PathBuf::from(""));
    }

    #[test]
    fn expand_tilde_uses_home_dir_when_available() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(expand_tilde("~/foo"), home.join("foo"));
        }
    }

    #[test]
    fn from_env_honors_ptywright_home_when_set() {
        // This test mutates global env; serialize via a process-wide mutex so
        // it does not race other tests that also touch PTYWRIGHT_HOME.
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        let prev = std::env::var_os(ENV_HOME);
        let target = unique_tempdir("from-env-set");
        // SAFETY: lock above keeps env mutation single-threaded for this test.
        unsafe { std::env::set_var(ENV_HOME, &target) };

        let paths = Paths::from_env();
        assert_eq!(paths.home(), target);

        // Restore prior state.
        // SAFETY: still under the env lock.
        unsafe {
            match prev {
                Some(value) => std::env::set_var(ENV_HOME, value),
                None => std::env::remove_var(ENV_HOME),
            }
        }
    }

    #[test]
    fn from_env_falls_back_to_home_dir_when_unset() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        let prev = std::env::var_os(ENV_HOME);
        // SAFETY: lock above serializes env mutation.
        unsafe { std::env::remove_var(ENV_HOME) };

        let paths = Paths::from_env();
        let expected = dirs::home_dir()
            .map(|home| home.join(DEFAULT_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR_NAME));
        assert_eq!(paths.home(), expected);

        // SAFETY: still under the env lock.
        unsafe {
            if let Some(value) = prev {
                std::env::set_var(ENV_HOME, value);
            }
        }
    }

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
}
