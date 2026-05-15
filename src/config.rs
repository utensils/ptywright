//! On-disk configuration for ptywright.
//!
//! ptywright reads `~/.ptywright/config.toml` when present. Missing files,
//! missing sections, and missing keys all fall back to sensible defaults so
//! the binary is usable on a fresh machine without any setup.
//!
//! Forward compatibility: unknown top-level keys are tolerated so older
//! binaries still load configs written by newer ones.

use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Top-level configuration model.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Logging-related settings.
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl Config {
    /// Load `~/.ptywright/config.toml` (or the equivalent under
    /// `PTYWRIGHT_HOME`), returning [`Self::default`] when the file does not
    /// exist. Any other I/O or parse failure is surfaced as [`Error::Config`].
    pub fn load_or_default(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml_str(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(Error::Config(format!(
                "failed to read {}: {error}",
                path.display()
            ))),
        }
    }

    /// Parse a TOML string into a [`Config`]. Used by tests and callers that
    /// already have the bytes in hand.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|error| Error::Config(error.to_string()))
    }
}

/// Logging configuration block (`[logging]` in TOML).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LoggingConfig {
    /// `tracing-subscriber::EnvFilter` directive (e.g. `"warn"`,
    /// `"info,ptywright::rpc=debug"`). Defaults to `"warn"`.
    #[serde(default = "LoggingConfig::default_level")]
    pub level: String,
    /// Whether to write rotated log files under `<home>/logs/`.
    #[serde(default = "LoggingConfig::default_file")]
    pub file: bool,
    /// Maximum age in days for log files retained on disk.
    #[serde(default = "LoggingConfig::default_max_days")]
    pub max_days: u32,
    /// Output format for log records.
    #[serde(default)]
    pub format: LogFormat,
}

impl LoggingConfig {
    fn default_level() -> String {
        "warn".to_string()
    }
    const fn default_file() -> bool {
        true
    }
    const fn default_max_days() -> u32 {
        14
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: Self::default_level(),
            file: Self::default_file(),
            max_days: Self::default_max_days(),
            format: LogFormat::default(),
        }
    }
}

/// Output format for log records.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable text (default).
    #[default]
    Text,
    /// JSON, one record per line.
    Json,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tempdir(label: &str) -> std::path::PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ptywright-config-{label}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn defaults_match_documented_values() {
        let config = Config::default();
        assert_eq!(config.logging.level, "warn");
        assert!(config.logging.file);
        assert_eq!(config.logging.max_days, 14);
        assert_eq!(config.logging.format, LogFormat::Text);
    }

    #[test]
    fn missing_file_returns_defaults_without_error() {
        let dir = unique_tempdir("missing");
        let path = dir.join("config.toml");
        let config = Config::load_or_default(&path).expect("missing file is not an error");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn partial_file_merges_with_defaults() {
        let text = r#"
[logging]
level = "info"
"#;
        let config = Config::from_toml_str(text).expect("parse partial config");
        assert_eq!(config.logging.level, "info");
        // Other fields fall back to defaults.
        assert!(config.logging.file);
        assert_eq!(config.logging.max_days, 14);
        assert_eq!(config.logging.format, LogFormat::Text);
    }

    #[test]
    fn json_format_is_recognized() {
        let text = r#"
[logging]
format = "json"
"#;
        let config = Config::from_toml_str(text).expect("parse json format");
        assert_eq!(config.logging.format, LogFormat::Json);
    }

    #[test]
    fn unknown_top_level_keys_are_tolerated() {
        let text = r#"
some_future_key = "ignored"

[logging]
level = "debug"

[some_future_section]
nested = true
"#;
        let config = Config::from_toml_str(text).expect("forward-compatible parse");
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn malformed_file_returns_config_error() {
        let text = "not = valid = toml";
        let error = Config::from_toml_str(text).expect_err("malformed should fail");
        assert!(matches!(error, Error::Config(_)));
    }

    #[test]
    fn load_from_real_file_round_trips() {
        let dir = unique_tempdir("roundtrip");
        std::fs::create_dir_all(&dir).expect("mk tempdir");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[logging]
level = "trace"
file = false
max_days = 3
format = "json"
"#,
        )
        .expect("write config");

        let config = Config::load_or_default(&path).expect("load");
        assert_eq!(config.logging.level, "trace");
        assert!(!config.logging.file);
        assert_eq!(config.logging.max_days, 3);
        assert_eq!(config.logging.format, LogFormat::Json);

        let _ = std::fs::remove_dir_all(dir);
    }
}
