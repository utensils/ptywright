use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

/// Policy for redacting sensitive text from snapshots, transcripts, and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionPolicy {
    /// Whether redaction is enabled.
    pub enabled: bool,
    /// Replacement marker used for sensitive values.
    pub replacement: String,
    /// Additional literal values to redact after built-in patterns.
    #[serde(default)]
    pub extra_literals: Vec<String>,
    /// Additional regular expressions to redact after built-in patterns.
    #[serde(default)]
    pub extra_regexes: Vec<String>,
}

impl RedactionPolicy {
    /// Create a redaction policy with the default replacement marker.
    #[must_use]
    pub fn enabled() -> Self {
        Self::default()
    }

    /// Create a policy that leaves text unchanged.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            replacement: "[REDACTED]".to_string(),
            extra_literals: Vec::new(),
            extra_regexes: Vec::new(),
        }
    }

    /// Validate caller-supplied regexes before using this policy in a fallible boundary.
    pub fn validate(&self) -> std::result::Result<(), String> {
        for pattern in &self.extra_regexes {
            Regex::new(pattern)
                .map_err(|error| format!("invalid redaction regex `{pattern}`: {error}"))?;
        }
        Ok(())
    }

    /// Redact sensitive-looking values from text.
    #[must_use]
    pub fn redact(&self, input: &str) -> String {
        if !self.enabled || input.is_empty() {
            return input.to_string();
        }

        let mut output = Cow::Borrowed(input);
        for regex in token_patterns() {
            output = Cow::Owned(
                regex
                    .replace_all(&output, |captures: &Captures<'_>| {
                        if let Some(prefix) = captures.name("prefix") {
                            format!("{}{}", prefix.as_str(), self.replacement)
                        } else {
                            self.replacement.clone()
                        }
                    })
                    .into_owned(),
            );
        }
        for regex in assignment_patterns() {
            output = Cow::Owned(
                regex
                    .replace_all(&output, |captures: &Captures<'_>| {
                        format!("{}{}", &captures["prefix"], self.replacement)
                    })
                    .into_owned(),
            );
        }
        for literal in &self.extra_literals {
            if !literal.is_empty() {
                output = Cow::Owned(output.replace(literal, &self.replacement));
            }
        }
        for pattern in &self.extra_regexes {
            if let Some(regex) = cached_extra_regex(pattern) {
                output = Cow::Owned(
                    regex
                        .replace_all(&output, self.replacement.as_str())
                        .into_owned(),
                );
            }
        }
        output.into_owned()
    }
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            replacement: "[REDACTED]".to_string(),
            extra_literals: Vec::new(),
            extra_regexes: Vec::new(),
        }
    }
}

fn cached_extra_regex(pattern: &str) -> Option<Regex> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Regex>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("redaction regex cache poisoned");
    if let Some(regex) = cache.get(pattern) {
        return regex.clone();
    }
    let regex = Regex::new(pattern).ok();
    cache.insert(pattern.to_string(), regex.clone());
    regex
}

fn assignment_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            Regex::new(
                r#"(?ix)
            (?P<prefix>\b(?:api[_-]?key|token|secret|password|passwd|authorization)\b\s*[:=]\s*)
            (?P<secret>[^\s,'"\]}]+)
            "#,
            )
            .expect("valid redaction regex"),
        ]
    })
}

fn token_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r#"(?i)(?P<prefix>\bbearer\s+)(?P<secret>[A-Za-z0-9._~+/=-]{8,})"#)
                .expect("valid bearer redaction regex"),
            Regex::new(r#"\bsk-[A-Za-z0-9_-]{16,}\b"#).expect("valid API key redaction regex"),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_assignments_and_tokens() {
        let policy = RedactionPolicy::default();
        let input = "api_key=abc123 token: xyz789 Authorization=Bearer abcdefghijkl sk-1234567890abcdefghijkl";

        let output = policy.redact(input);

        assert!(!output.contains("abc123"));
        assert!(!output.contains("xyz789"));
        assert!(!output.contains("abcdefghijkl"));
        assert!(output.contains("api_key=[REDACTED]"));
        assert!(output.contains("token: [REDACTED]"));
    }

    #[test]
    fn disabled_policy_leaves_text_unchanged() {
        let input = "password=hunter2";

        assert_eq!(RedactionPolicy::disabled().redact(input), input);
    }

    #[test]
    fn redacts_user_configured_literals_and_regexes() {
        let policy = RedactionPolicy {
            extra_literals: vec!["project-secret".to_string()],
            extra_regexes: vec![r"internal-[0-9]+".to_string()],
            ..RedactionPolicy::default()
        };

        policy.validate().expect("valid custom redaction regex");
        let output = policy.redact("project-secret internal-123 public");

        assert_eq!(output, "[REDACTED] [REDACTED] public");
    }

    #[test]
    fn validates_user_configured_regexes() {
        let policy = RedactionPolicy {
            extra_regexes: vec!["(".to_string()],
            ..RedactionPolicy::default()
        };

        let error = policy.validate().expect_err("invalid regex should fail");

        assert!(error.contains("invalid redaction regex"));
    }
}
