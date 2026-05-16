use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::error::Result;

/// Configuration for optional raw transcript file streaming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFileConfig {
    /// File path to stream raw PTY bytes into.
    pub path: PathBuf,
    /// Append to an existing file instead of creating a new file exclusively.
    pub append: bool,
}

impl TranscriptFileConfig {
    /// Stream raw transcript bytes to a new file, refusing to overwrite by default.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            append: false,
        }
    }

    /// Allow appending to an existing raw transcript file.
    #[must_use]
    pub const fn append(mut self, append: bool) -> Self {
        self.append = append;
        self
    }
}

/// Configuration for bounded transcript retention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptConfig {
    /// Maximum UTF-8 scalar values retained in memory.
    pub max_chars: usize,
    /// Optional explicit raw/unredacted transcript file stream.
    pub raw_file: Option<TranscriptFileConfig>,
}

impl Default for TranscriptConfig {
    fn default() -> Self {
        Self {
            max_chars: 128 * 1024,
            raw_file: None,
        }
    }
}

/// Bounded text transcript of PTY output, with optional raw byte file streaming.
#[derive(Debug)]
pub struct Transcript {
    config: TranscriptConfig,
    chars: VecDeque<char>,
    /// Total chars ever pushed, never decremented when the ring buffer evicts.
    /// Stable cursor for subscribers asking "what's new since cursor N?" —
    /// see [`Transcript::delta_since`].
    chars_written: u64,
    raw_file: Option<File>,
}

/// Output appended to a [`Transcript`] since a subscriber's cursor.
///
/// `dropped` is set when the unseen range exceeded the ring buffer's retention
/// window — `text` then carries the buffer tail rather than the full delta.
#[derive(Debug, Clone)]
pub struct TranscriptDelta {
    /// Newly-appended text since the caller's cursor.
    pub text: String,
    /// Cursor to pass on the next call to advance past this delta.
    pub cursor: u64,
    /// True if the bounded buffer dropped some of the unseen range before we
    /// could return it. `text` is the (smaller) tail that survived.
    pub dropped: bool,
}

impl Transcript {
    /// Create a transcript with the provided retention config.
    pub fn new(config: TranscriptConfig) -> Result<Self> {
        let raw_file = config
            .raw_file
            .as_ref()
            .map(open_raw_transcript_file)
            .transpose()?;
        Ok(Self {
            config,
            chars: VecDeque::new(),
            chars_written: 0,
            raw_file,
        })
    }

    /// Append output bytes using lossy UTF-8 decoding and optional raw file streaming.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(file) = &mut self.raw_file {
            file.write_all(bytes)?;
        }

        let text = String::from_utf8_lossy(bytes);
        for ch in text.chars() {
            self.chars.push_back(ch);
            self.chars_written = self.chars_written.saturating_add(1);
            while self.chars.len() > self.config.max_chars {
                self.chars.pop_front();
            }
        }
        Ok(())
    }

    /// Return the retained transcript text.
    #[must_use]
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Return the tail of the retained transcript.
    #[must_use]
    pub fn tail(&self, max_chars: usize) -> String {
        let len = self.chars.len();
        self.chars
            .iter()
            .skip(len.saturating_sub(max_chars))
            .collect()
    }

    /// Total chars ever pushed since this transcript was created. Survives
    /// ring-buffer evictions so subscribers can carry a stable cursor.
    #[must_use]
    pub const fn chars_written(&self) -> u64 {
        self.chars_written
    }

    /// Text appended since `cursor`. Returns an empty delta when the caller is
    /// already at `chars_written()`.
    #[must_use]
    pub fn delta_since(&self, cursor: u64) -> TranscriptDelta {
        let total = self.chars_written;
        if cursor >= total {
            return TranscriptDelta {
                text: String::new(),
                cursor: total,
                dropped: false,
            };
        }
        let unseen = (total - cursor) as usize;
        let buffered = self.chars.len();
        let (text, dropped) = if unseen > buffered {
            (self.chars.iter().collect(), true)
        } else {
            (self.chars.iter().skip(buffered - unseen).collect(), false)
        };
        TranscriptDelta {
            text,
            cursor: total,
            dropped,
        }
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new(TranscriptConfig::default()).expect("default transcript config is valid")
    }
}

fn open_raw_transcript_file(config: &TranscriptFileConfig) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    if config.append {
        options.create(true).append(true);
    } else {
        options.create_new(true);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    options.open(&config.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_retains_bounded_tail() {
        let mut transcript = Transcript::new(TranscriptConfig {
            max_chars: 5,
            raw_file: None,
        })
        .expect("create transcript");

        transcript.push_bytes(b"hello").expect("push bytes");
        transcript.push_bytes(b" world").expect("push bytes");

        assert_eq!(transcript.text(), "world");
        assert_eq!(transcript.tail(3), "rld");
    }

    #[test]
    fn delta_since_returns_appended_text_and_advances_cursor() {
        let mut transcript = Transcript::default();
        let initial = transcript.delta_since(0);
        assert_eq!(initial.text, "");
        assert_eq!(initial.cursor, 0);
        assert!(!initial.dropped);

        transcript.push_bytes(b"hello").expect("push hello");
        let after_hello = transcript.delta_since(0);
        assert_eq!(after_hello.text, "hello");
        assert_eq!(after_hello.cursor, 5);
        assert!(!after_hello.dropped);

        transcript.push_bytes(b" world").expect("push world");
        let after_world = transcript.delta_since(after_hello.cursor);
        assert_eq!(after_world.text, " world");
        assert_eq!(after_world.cursor, 11);
        assert!(!after_world.dropped);

        let idempotent = transcript.delta_since(after_world.cursor);
        assert_eq!(idempotent.text, "");
        assert_eq!(idempotent.cursor, 11);
    }

    #[test]
    fn delta_since_flags_dropped_when_unseen_range_exceeds_buffer() {
        // Tiny ring buffer so eviction is easy to trigger. `chars_written`
        // still counts every push, so the cursor stays meaningful — the delta
        // just flags that some of the unseen range was lost.
        let mut transcript = Transcript::new(TranscriptConfig {
            max_chars: 4,
            raw_file: None,
        })
        .expect("create transcript");
        transcript.push_bytes(b"abcdefgh").expect("push bytes");

        let delta = transcript.delta_since(0);
        assert!(delta.dropped, "lossy delta should flag dropped=true");
        assert_eq!(
            delta.text, "efgh",
            "dropped delta carries the surviving tail"
        );
        assert_eq!(delta.cursor, 8);
    }

    #[test]
    fn transcript_streams_raw_bytes_to_explicit_file_without_overwrite() {
        let path = std::env::temp_dir().join(format!(
            "ptywright-transcript-{}-{}.log",
            std::process::id(),
            unique_suffix()
        ));
        let mut transcript = Transcript::new(TranscriptConfig {
            max_chars: 5,
            raw_file: Some(TranscriptFileConfig::new(&path)),
        })
        .expect("create raw transcript");

        transcript
            .push_bytes(b"token=super-secret\n")
            .expect("stream raw bytes");
        drop(transcript);

        let bytes = std::fs::read(&path).expect("read raw transcript file");
        assert_eq!(bytes, b"token=super-secret\n");
        let error = Transcript::new(TranscriptConfig {
            max_chars: 5,
            raw_file: Some(TranscriptFileConfig::new(&path)),
        })
        .expect_err("existing file should not be overwritten");
        assert!(matches!(
            error,
            crate::error::Error::Io(ref io_error)
                if io_error.kind() == std::io::ErrorKind::AlreadyExists
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn transcript_can_append_to_explicit_raw_file() {
        let path = std::env::temp_dir().join(format!(
            "ptywright-transcript-{}-{}.log",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::write(&path, b"first\n").expect("seed transcript");
        let mut transcript = Transcript::new(TranscriptConfig {
            max_chars: 16,
            raw_file: Some(TranscriptFileConfig::new(&path).append(true)),
        })
        .expect("open append transcript");

        transcript.push_bytes(b"second\n").expect("append bytes");
        drop(transcript);

        let bytes = std::fs::read(&path).expect("read raw transcript file");
        assert_eq!(bytes, b"first\nsecond\n");
        let _ = std::fs::remove_file(path);
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos()
    }

    #[test]
    #[cfg(unix)]
    fn raw_transcript_file_is_created_with_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "ptywright-transcript-mode-{}-{}.log",
            std::process::id(),
            unique_suffix()
        ));
        let transcript = Transcript::new(TranscriptConfig {
            max_chars: 16,
            raw_file: Some(TranscriptFileConfig::new(&path)),
        })
        .expect("create raw transcript");
        drop(transcript);

        let metadata = std::fs::metadata(&path).expect("stat raw transcript");
        // Raw transcripts are unredacted sensitive data: SPEC's resolved
        // design decisions promise "restrictive permissions where supported".
        // Mask the file-type bits and require owner-read/write only.
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "raw transcript permissions must be 0o600 on Unix, got {mode:o}"
        );

        let _ = std::fs::remove_file(path);
    }
}
