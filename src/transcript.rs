use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::error::Result;

/// Maximum number of distinct marker labels retained per transcript.
/// Plugins typically need only a handful (`prompt_submitted`,
/// `turn_complete`, …); the cap keeps marker storage from growing
/// unboundedly if a plugin author writes a label-per-event by mistake.
const MAX_MARKERS: usize = 64;

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
    /// Caller-placed markers: label → char cursor at the time
    /// [`Transcript::mark`] was called. Multiple marks with the same
    /// label overwrite (most recent wins). Capped at [`MAX_MARKERS`].
    marks: BTreeMap<String, u64>,
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
            marks: BTreeMap::new(),
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

    /// Record a marker at the current write cursor and return it. Labels
    /// are arbitrary strings; later [`Transcript::slice_between`] /
    /// [`Transcript::marker`] lookups can use the returned cursor or the
    /// label.
    ///
    /// Repeated calls with the same label overwrite (most-recent wins) —
    /// plugins that re-enter a state can call `mark("turn_complete")` on
    /// each entry without leaking storage. The marker table is capped at
    /// [`MAX_MARKERS`] distinct labels; once full, additional **new**
    /// labels are rejected (existing labels still update). The returned
    /// cursor is the write position regardless — callers that need to
    /// detect rejection should pair this with [`Transcript::marker`].
    /// A rejection is also logged at `tracing::warn` level under the
    /// `ptywright::transcript` target so a plugin author that hits the
    /// cap learns about it without having to re-query.
    pub fn mark(&mut self, label: impl Into<String>) -> u64 {
        let cursor = self.chars_written;
        let label = label.into();
        if self.marks.contains_key(&label) || self.marks.len() < MAX_MARKERS {
            self.marks.insert(label, cursor);
        } else {
            tracing::warn!(
                target: "ptywright::transcript",
                label = %label,
                max_markers = MAX_MARKERS,
                "transcript marker rejected: label table is full"
            );
        }
        cursor
    }

    /// Cursor previously placed at `label`, or `None` if no such marker
    /// exists.
    #[must_use]
    pub fn marker(&self, label: &str) -> Option<u64> {
        self.marks.get(label).copied()
    }

    /// Read all currently-recorded markers. Returns a borrow rather than a
    /// clone so hot classifier paths that only need read access do not
    /// allocate.
    ///
    /// Plugin classifiers receive this view through
    /// [`crate::ClassifyContext::markers`] so they can compose transcript
    /// metadata (e.g. `metadata.transcript = { turn_start, turn_end }`)
    /// without round-tripping through individual [`marker`](Self::marker)
    /// calls.
    #[must_use]
    pub fn markers(&self) -> &BTreeMap<String, u64> {
        &self.marks
    }

    /// Text between two byte cursors, or `None` if either cursor refers
    /// to text the bounded ring buffer has already evicted.
    ///
    /// `a` and `b` may be passed in either order; the returned text
    /// reads from the lower to the higher cursor regardless. Both
    /// cursors must be `<= chars_written()`; otherwise this returns
    /// `None`.
    ///
    /// Pair with [`Transcript::mark`] to segment turn-bounded
    /// transcripts: a plugin that marks `turn_start` on prompt submit
    /// and `turn_end` on completion can later ask
    /// `slice_between(turn_start, turn_end)` for the exact bytes
    /// belonging to that turn.
    #[must_use]
    pub fn slice_between(&self, a: u64, b: u64) -> Option<String> {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let total = self.chars_written;
        if hi > total {
            return None;
        }
        let buffered = self.chars.len() as u64;
        let earliest_retained = total.saturating_sub(buffered);
        if lo < earliest_retained {
            return None;
        }
        let skip = usize::try_from(lo - earliest_retained).ok()?;
        let take = usize::try_from(hi - lo).ok()?;
        Some(self.chars.iter().skip(skip).take(take).collect())
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
        // Saturate the gap to `usize::MAX` so the comparison below stays
        // correct on 32-bit targets even when the unseen range exceeds
        // `usize::MAX` characters (≈ 4 GiB on a 32-bit usize). The ring
        // buffer is bounded so we'll fall into the `dropped` branch anyway —
        // we just must not let `as usize` truncate the difference to a
        // smaller value and silently return a non-dropped slice.
        let unseen = usize::try_from(total - cursor).unwrap_or(usize::MAX);
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

    #[test]
    fn mark_and_slice_between_returns_the_marked_range() {
        let mut transcript = Transcript::default();
        transcript.push_bytes(b"prompt\n").expect("push prompt");
        let turn_start = transcript.mark("turn_start");
        transcript
            .push_bytes(b"reply with tokens\n")
            .expect("push reply");
        let turn_end = transcript.mark("turn_end");
        transcript
            .push_bytes(b"trailing chrome")
            .expect("push trailing");

        assert_eq!(transcript.marker("turn_start"), Some(turn_start));
        assert_eq!(transcript.marker("turn_end"), Some(turn_end));
        assert_eq!(
            transcript.slice_between(turn_start, turn_end).as_deref(),
            Some("reply with tokens\n"),
            "slice between markers must include the exact byte range"
        );
        // Argument order is irrelevant — slice_between sorts internally.
        assert_eq!(
            transcript.slice_between(turn_end, turn_start).as_deref(),
            Some("reply with tokens\n"),
        );
    }

    #[test]
    fn mark_overwrites_same_label() {
        // Plugin pattern: re-enter `completed_turn`, mark again. Second
        // call must overwrite without growing the marker table.
        let mut transcript = Transcript::default();
        let first = transcript.mark("turn_complete");
        transcript.push_bytes(b"xxxxxxxx").expect("push pad");
        let second = transcript.mark("turn_complete");
        assert!(second > first);
        assert_eq!(transcript.marker("turn_complete"), Some(second));
    }

    #[test]
    fn slice_between_returns_none_when_cursor_evicted() {
        // Tiny ring forces eviction. The marker remains in the table
        // (chars_written is monotonic) but its cursor refers to bytes
        // the ring no longer holds — `slice_between` must signal that
        // by returning None rather than a misleading partial slice.
        let mut transcript = Transcript::new(TranscriptConfig {
            max_chars: 4,
            raw_file: None,
        })
        .expect("create transcript");
        let early = transcript.mark("early");
        transcript.push_bytes(b"abcdefgh").expect("push");
        let late = transcript.mark("late");
        assert_eq!(transcript.slice_between(early, late), None);
    }

    #[test]
    fn slice_between_returns_none_for_cursor_beyond_written() {
        let transcript = Transcript::default();
        // Marker that hasn't existed: clearly out of bounds.
        assert_eq!(transcript.slice_between(0, 100), None);
    }

    #[test]
    fn marker_cap_rejects_new_labels_when_full() {
        // Defensive: a plugin author that accidentally writes a
        // label-per-event must not be able to grow the table without
        // bound. After MAX_MARKERS distinct labels, additional ones
        // are dropped while existing labels still update normally.
        let mut transcript = Transcript::default();
        for i in 0..MAX_MARKERS {
            transcript.mark(format!("label-{i}"));
        }
        assert_eq!(transcript.marker("label-0"), Some(0));
        transcript.push_bytes(b"x").expect("push");
        transcript.mark("overflow-label");
        assert_eq!(
            transcript.marker("overflow-label"),
            None,
            "new labels past MAX_MARKERS must be rejected"
        );
        // Existing labels still update.
        let bumped = transcript.mark("label-0");
        assert_eq!(bumped, 1);
        assert_eq!(transcript.marker("label-0"), Some(1));
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
