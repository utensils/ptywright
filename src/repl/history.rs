//! File-backed line history for the REPL.
//!
//! Plain `VecDeque<String>` rolling buffer flushed to disk on every push.
//! Used by the ratatui TUI's input box: Up/Down arrow keys cycle through
//! entries newest-first. The on-disk format is one entry per line, oldest
//! first, capped at `DEFAULT_CAPACITY` lines so the file does not grow
//! without bound.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Default capacity for the on-disk history file. Each entry is one
/// command line, so 1000 entries comfortably covers months of casual use
/// without bloating the file.
pub const DEFAULT_CAPACITY: usize = 1000;

/// Bounded line history persisted to disk.
#[derive(Debug)]
pub struct ReplHistory {
    path: PathBuf,
    entries: VecDeque<String>,
    capacity: usize,
}

impl ReplHistory {
    /// Open (or create) a history file at `path`. Existing entries are
    /// loaded into memory; the parent directory is created on demand.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::from)?;
        }
        let entries = read_lines(path)?;
        let mut deque = VecDeque::with_capacity(entries.len().max(16));
        deque.extend(entries);
        Ok(Self {
            path: path.to_path_buf(),
            entries: deque,
            capacity: DEFAULT_CAPACITY,
        })
    }

    /// Record one command, deduping with the most-recent entry and
    /// trimming the deque to capacity before flushing.
    pub fn push(&mut self, line: &str) -> Result<()> {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Ok(());
        }
        if self.entries.back().map(String::as_str) == Some(trimmed) {
            return Ok(());
        }
        self.entries.push_back(trimmed.to_string());
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
        self.flush()
    }

    /// All entries, oldest first.
    pub fn entries(&self) -> &VecDeque<String> {
        &self.entries
    }

    /// Number of entries in memory.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the history is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Fetch the n-th entry counting back from the newest (0 = most
    /// recent). Returns `None` when `n` exceeds the buffer.
    pub fn nth_back(&self, n: usize) -> Option<&str> {
        if n >= self.entries.len() {
            return None;
        }
        self.entries
            .get(self.entries.len() - 1 - n)
            .map(String::as_str)
    }

    fn flush(&self) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .map_err(Error::from)?;
        for entry in &self.entries {
            writeln!(file, "{entry}").map_err(Error::from)?;
        }
        Ok(())
    }
}

fn read_lines(path: &Path) -> Result<Vec<String>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::from(error)),
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(Error::from)?;
        if !line.is_empty() {
            out.push(line);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tempfile(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ptywright-repl-history-{}-{}-{suffix}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ))
    }

    #[test]
    fn open_creates_parent_directory_and_file() {
        let dir = unique_tempfile("dir");
        let path = dir.join("history");
        let history = ReplHistory::open(&path).expect("open new history file");
        assert!(history.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn push_persists_and_dedupes_consecutive_entries() {
        let path = unique_tempfile("push");
        let mut history = ReplHistory::open(&path).expect("open");
        history.push("alpha").expect("push");
        history.push("alpha").expect("push dedup");
        history.push("beta").expect("push");
        assert_eq!(history.len(), 2);
        let reopened = ReplHistory::open(&path).expect("reopen");
        assert_eq!(
            reopened
                .entries()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn nth_back_walks_newest_first() {
        let path = unique_tempfile("nth");
        let mut history = ReplHistory::open(&path).expect("open");
        history.push("one").expect("push");
        history.push("two").expect("push");
        history.push("three").expect("push");
        assert_eq!(history.nth_back(0), Some("three"));
        assert_eq!(history.nth_back(1), Some("two"));
        assert_eq!(history.nth_back(2), Some("one"));
        assert_eq!(history.nth_back(3), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn push_trims_to_capacity() {
        let path = unique_tempfile("cap");
        let mut history = ReplHistory::open(&path).expect("open");
        history.capacity = 3;
        for n in 0..5 {
            history.push(&format!("cmd-{n}")).expect("push");
        }
        assert_eq!(history.len(), 3);
        assert_eq!(history.nth_back(0), Some("cmd-4"));
        assert_eq!(history.nth_back(2), Some("cmd-2"));
        let _ = std::fs::remove_file(&path);
    }
}
