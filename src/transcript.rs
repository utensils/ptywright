use std::collections::VecDeque;

/// Configuration for bounded transcript retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptConfig {
    /// Maximum UTF-8 scalar values retained in memory.
    pub max_chars: usize,
}

impl Default for TranscriptConfig {
    fn default() -> Self {
        Self {
            max_chars: 128 * 1024,
        }
    }
}

/// Bounded text transcript of PTY output.
#[derive(Debug, Clone)]
pub struct Transcript {
    config: TranscriptConfig,
    chars: VecDeque<char>,
}

impl Transcript {
    /// Create a transcript with the provided retention config.
    #[must_use]
    pub fn new(config: TranscriptConfig) -> Self {
        Self {
            config,
            chars: VecDeque::new(),
        }
    }

    /// Append output bytes using lossy UTF-8 decoding.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        for ch in text.chars() {
            self.chars.push_back(ch);
            while self.chars.len() > self.config.max_chars {
                self.chars.pop_front();
            }
        }
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
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new(TranscriptConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_retains_bounded_tail() {
        let mut transcript = Transcript::new(TranscriptConfig { max_chars: 5 });

        transcript.push_bytes(b"hello");
        transcript.push_bytes(b" world");

        assert_eq!(transcript.text(), "world");
        assert_eq!(transcript.tail(3), "rld");
    }
}
