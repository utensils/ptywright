//! Persistent line-editor history for the REPL.
//!
//! Thin wrapper around `reedline::FileBackedHistory` rooted at
//! `Paths::repl_history_path()`. Lifted into its own module so future
//! callers (search, history-pane rendering) have one place to look.

use std::path::Path;

use reedline::FileBackedHistory;

use crate::error::{Error, Result};

/// Default capacity for the on-disk history file. Each entry is one
/// command line, so 1000 entries comfortably covers months of casual use
/// without bloating the file.
pub const DEFAULT_CAPACITY: usize = 1000;

/// Build a reedline `FileBackedHistory` rooted at `path`. Creates parent
/// directories on demand so the first run after a fresh install does not
/// fail to open the file.
pub fn open(path: &Path) -> Result<FileBackedHistory> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::from)?;
    }
    FileBackedHistory::with_file(DEFAULT_CAPACITY, path.to_path_buf())
        .map_err(|error| Error::Rpc(format!("open repl history `{}`: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_parent_directory_and_file() {
        let dir = std::env::temp_dir().join(format!(
            "ptywright-repl-history-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let path = dir.join("history");
        let _history = open(&path).expect("open new history file");
        assert!(path.exists(), "reedline should create the history file");
        let _ = std::fs::remove_dir_all(dir);
    }
}
