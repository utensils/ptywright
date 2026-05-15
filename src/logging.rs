//! Structured logging for ptywright.
//!
//! All ptywright modes share the same on-disk log layout:
//!
//! - Daily-rotated files under `<paths.home()>/logs/`, prefixed `ptywright`,
//!   suffixed `log` (e.g. `ptywright.2026-05-14.log`).
//! - Bounded retention: files older than [`LoggingConfig::max_days`] are
//!   removed at init.
//! - All log records pass through [`RedactionPolicy`] before being written so
//!   ptywright-owned diagnostics never leak secrets to disk or stderr.
//!
//! Mode-specific init functions pick the right sink combination so the CLI's
//! output contracts are upheld:
//!
//! - [`init_for_run`] is file-only — `ptywright run` bridges raw bytes to the
//!   user's terminal, so any extra stderr would corrupt the live session.
//! - [`init_for_serve_stdio`] writes to file and stderr; stdout is reserved
//!   for JSON-RPC framing and is never touched.
//! - [`init_for_serve_socket`] writes to file and stderr.
//! - [`init_for_oneshot`] writes to stderr only and skips file setup, for
//!   short-lived commands like `--help`, `--version`, and `completions`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

use crate::config::{LogFormat, LoggingConfig};
use crate::paths::Paths;
use crate::redaction::RedactionPolicy;

/// Environment variable that overrides [`LoggingConfig::level`].
pub const ENV_FILTER: &str = "PTYWRIGHT_LOG";

/// Filename prefix used for rotated log files.
const LOG_PREFIX: &str = "ptywright";
/// Filename suffix used for rotated log files.
const LOG_SUFFIX: &str = "log";

/// RAII handle for a configured logging stack.
///
/// Holds the [`tracing_appender`] worker guard so the background flush thread
/// stays alive for the duration of the program. Drop the guard at exit to
/// flush remaining records.
#[must_use = "drop the LogGuard at process exit to flush pending log records"]
pub struct LogGuard {
    _file_guard: Option<WorkerGuard>,
}

impl LogGuard {
    /// Returns a guard that owns no background workers.
    fn empty() -> Self {
        Self { _file_guard: None }
    }
}

/// Install logging for `ptywright run`. File-only — the command owns the
/// user's terminal and stderr would corrupt the live PTY bridge.
pub fn init_for_run(paths: &Paths, config: &LoggingConfig) -> LogGuard {
    let policy = RedactionPolicy::default();
    install_with_sinks(paths, config, SinkSelection::FileOnly, Arc::new(policy))
}

/// Install logging for `ptywright serve --stdio`. Writes to a rotated log
/// file and stderr. Stdout is reserved for JSON-RPC and is never written here.
pub fn init_for_serve_stdio(paths: &Paths, config: &LoggingConfig) -> LogGuard {
    install_with_sinks(
        paths,
        config,
        SinkSelection::FileAndStderr,
        Arc::new(RedactionPolicy::default()),
    )
}

/// Install logging for `ptywright serve --socket`. Writes to a rotated log
/// file and stderr.
pub fn init_for_serve_socket(paths: &Paths, config: &LoggingConfig) -> LogGuard {
    install_with_sinks(
        paths,
        config,
        SinkSelection::FileAndStderr,
        Arc::new(RedactionPolicy::default()),
    )
}

/// Install minimal logging for short-lived commands (`--help`, `--version`,
/// `completions`). Writes redacted records to stderr only.
pub fn init_for_oneshot(config: &LoggingConfig) -> LogGuard {
    let filter = make_filter(config);
    let policy = Arc::new(RedactionPolicy::default());
    let stderr_writer = RedactingMakeWriter::new(io::stderr, policy);

    let layer = tracing_subscriber::fmt::layer()
        .with_writer(stderr_writer)
        .with_filter(filter);
    let _ = tracing_subscriber::registry().with(layer).try_init();
    LogGuard::empty()
}

#[derive(Debug, Clone, Copy)]
enum SinkSelection {
    FileOnly,
    FileAndStderr,
}

fn install_with_sinks(
    paths: &Paths,
    config: &LoggingConfig,
    sinks: SinkSelection,
    policy: Arc<RedactionPolicy>,
) -> LogGuard {
    let log_dir = paths.logs_dir();
    let file_setup = if config.file {
        prepare_file_appender(&log_dir, config.max_days)
    } else {
        None
    };

    match (sinks, file_setup) {
        (SinkSelection::FileOnly, Some((non_blocking, guard))) => {
            install_layers(config, Some((non_blocking, policy.clone())), None);
            LogGuard {
                _file_guard: Some(guard),
            }
        }
        (SinkSelection::FileOnly, None) => {
            // File-only init with file disabled or appender failure: register
            // a no-op subscriber so tracing macros are silently dropped rather
            // than leaving the global subscriber unset.
            let _ = tracing_subscriber::registry().try_init();
            LogGuard::empty()
        }
        (SinkSelection::FileAndStderr, Some((non_blocking, guard))) => {
            install_layers(config, Some((non_blocking, policy.clone())), Some(policy));
            LogGuard {
                _file_guard: Some(guard),
            }
        }
        (SinkSelection::FileAndStderr, None) => {
            install_layers(config, None, Some(policy));
            LogGuard::empty()
        }
    }
}

fn install_layers(
    config: &LoggingConfig,
    file: Option<(
        tracing_appender::non_blocking::NonBlocking,
        Arc<RedactionPolicy>,
    )>,
    stderr: Option<Arc<RedactionPolicy>>,
) {
    let registry = tracing_subscriber::registry();

    let stderr_layer = stderr.map(|policy| {
        tracing_subscriber::fmt::layer()
            .with_writer(RedactingMakeWriter::new(io::stderr, policy))
            .with_filter(make_filter(config))
    });

    let file_layer_text = file.as_ref().and_then(|(non_blocking, policy)| {
        if matches!(config.format, LogFormat::Text) {
            Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(RedactingMakeWriter::new(
                        non_blocking.clone(),
                        policy.clone(),
                    ))
                    .with_filter(make_filter(config)),
            )
        } else {
            None
        }
    });

    let file_layer_json = file.and_then(|(non_blocking, policy)| {
        if matches!(config.format, LogFormat::Json) {
            Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .json()
                    .with_writer(RedactingMakeWriter::new(non_blocking, policy))
                    .with_filter(make_filter(config)),
            )
        } else {
            None
        }
    });

    let _ = registry
        .with(stderr_layer)
        .with(file_layer_text)
        .with(file_layer_json)
        .try_init();
}

fn prepare_file_appender(
    log_dir: &Path,
    max_days: u32,
) -> Option<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    // Silent best-effort: logging is never load-bearing. In particular,
    // `init_for_run` must not write to stderr or it will corrupt the user's
    // live PTY bridge — so failures here cannot be surfaced through eprintln.
    // Callers who need to confirm logging is working should inspect
    // <home>/logs/ directly.
    std::fs::create_dir_all(log_dir).ok()?;
    cleanup_old_logs(log_dir, max_days);

    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_PREFIX)
        .filename_suffix(LOG_SUFFIX)
        .build(log_dir)
        .ok()?;
    Some(tracing_appender::non_blocking(appender))
}

fn make_filter(config: &LoggingConfig) -> EnvFilter {
    let raw = std::env::var(ENV_FILTER).unwrap_or_else(|_| {
        if config.level.trim().is_empty() {
            "warn".to_string()
        } else {
            config.level.clone()
        }
    });
    EnvFilter::try_new(&raw).unwrap_or_else(|_| EnvFilter::new("warn"))
}

/// Delete log files older than `max_days` from `log_dir`. A `max_days` of 0
/// disables retention. Errors are swallowed; retention is best-effort.
pub fn cleanup_old_logs(log_dir: &Path, max_days: u32) {
    if max_days == 0 {
        return;
    }
    let now = SystemTime::now();
    let max_age = Duration::from_secs(u64::from(max_days) * 86_400);

    let entries = match std::fs::read_dir(log_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !is_managed_log(name) {
            continue;
        }
        if let Ok(metadata) = path.metadata()
            && let Ok(modified) = metadata.modified()
            && let Ok(age) = now.duration_since(modified)
            && age > max_age
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn is_managed_log(name: &str) -> bool {
    // RollingFileAppender emits "ptywright.<date>.log". Require the dot
    // separator after the prefix and the .log suffix so the sweep does not
    // touch unrelated files that merely share the bare prefix
    // (e.g. "ptywright-notes.txt").
    name.starts_with(LOG_PREFIX)
        && name.ends_with(LOG_SUFFIX)
        && name.as_bytes().get(LOG_PREFIX.len()) == Some(&b'.')
        && name.len() > LOG_PREFIX.len() + LOG_SUFFIX.len() + 1
}

/// `MakeWriter` adapter that redacts each formatted record before forwarding.
#[derive(Clone)]
pub struct RedactingMakeWriter<M> {
    inner: M,
    policy: Arc<RedactionPolicy>,
}

impl<M> RedactingMakeWriter<M> {
    /// Wrap `inner` so that each formatted record is run through `policy`.
    pub fn new(inner: M, policy: Arc<RedactionPolicy>) -> Self {
        Self { inner, policy }
    }
}

impl<'a, M> MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: MakeWriter<'a>,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
            policy: self.policy.clone(),
        }
    }
}

/// `Write` adapter that redacts each `write` call before forwarding.
///
/// Tracing's formatter buffers each record into a single `write` (or
/// `write_all`) per event, so per-call redaction is sufficient — there is no
/// need for line buffering across calls.
pub struct RedactingWriter<W> {
    inner: W,
    policy: Arc<RedactionPolicy>,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = std::str::from_utf8(buf).unwrap_or("");
        let redacted = self.policy.redact(text);
        // Use write_all: the redacted bytes do not correspond to any tail of
        // `buf`, so a caller observing a short return cannot retry the
        // remaining content. Looping inside the writer is the only way to
        // honor the Write contract without silently truncating log records.
        self.inner.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Helper used by tests and CLI smoke checks: compute the directory
/// preparation outcome without installing a global subscriber.
#[doc(hidden)]
pub fn ensure_log_dir_for_test(paths: &Paths) -> std::io::Result<PathBuf> {
    let dir = paths.logs_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    fn unique_tempdir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ptywright-logging-{label}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn cleanup_removes_old_files_and_keeps_recent_ones() {
        let dir = unique_tempdir("cleanup");
        fs::create_dir_all(&dir).expect("mk tempdir");

        let old = dir.join("ptywright.2024-01-01.log");
        let new = dir.join("ptywright.2026-05-14.log");
        let unrelated = dir.join("notes.txt");
        let prefixed_unrelated = dir.join("ptywright-notes.txt");
        let suffix_only = dir.join("server.log");
        fs::write(&old, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();
        fs::write(&prefixed_unrelated, b"prefixed").unwrap();
        fs::write(&suffix_only, b"suffix").unwrap();

        // Backdate every "old"-shaped file by 30 days. Surface the error if
        // mtime setting fails so the cleanup assertion can't pass for the
        // wrong reason.
        let thirty_days_ago = SystemTime::now() - Duration::from_secs(30 * 86_400);
        filetime_set_mtime(&old, thirty_days_ago).expect("backdate old log");
        filetime_set_mtime(&prefixed_unrelated, thirty_days_ago).expect("backdate prefixed file");
        filetime_set_mtime(&suffix_only, thirty_days_ago).expect("backdate suffix-only file");

        cleanup_old_logs(&dir, 7);

        assert!(!old.exists(), "old ptywright log should be deleted");
        assert!(new.exists(), "recent ptywright log should be retained");
        assert!(unrelated.exists(), "unrelated files must not be touched");
        assert!(
            prefixed_unrelated.exists(),
            "files with the bare 'ptywright' prefix but no '.<date>.log' shape must be preserved"
        );
        assert!(
            suffix_only.exists(),
            "files with the .log suffix but a different prefix must be preserved"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cleanup_with_zero_max_days_is_a_no_op() {
        let dir = unique_tempdir("zero");
        fs::create_dir_all(&dir).expect("mk tempdir");
        let old = dir.join("ptywright.2020-01-01.log");
        fs::write(&old, b"ancient").unwrap();
        let ten_years_ago = SystemTime::now() - Duration::from_secs(10 * 365 * 86_400);
        filetime_set_mtime(&old, ten_years_ago).expect("backdate old log");

        cleanup_old_logs(&dir, 0);

        assert!(old.exists(), "max_days=0 disables retention");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn is_managed_log_only_matches_appender_output_shape() {
        // Files the appender actually writes — both must match.
        assert!(super::is_managed_log("ptywright.2026-05-14.log"));
        assert!(super::is_managed_log("ptywright.2024-12-31.log"));
        // Files that share the prefix but are not log records — must not match.
        assert!(!super::is_managed_log("ptywright-notes.txt"));
        assert!(!super::is_managed_log("ptywright"));
        assert!(!super::is_managed_log("ptywright.log"));
        assert!(!super::is_managed_log("ptywright.txt"));
        // Unrelated files — must not match.
        assert!(!super::is_managed_log("server.log"));
        assert!(!super::is_managed_log("notes.txt"));
    }

    #[test]
    fn cleanup_ignores_missing_directory() {
        let dir = unique_tempdir("missing");
        // Do not create — function must not panic on ENOENT.
        cleanup_old_logs(&dir, 7);
    }

    #[test]
    fn redacting_writer_handles_partial_inner_writes_without_truncation() {
        // Inner writer that intentionally accepts only one byte per call,
        // simulating a sink (e.g. a non-blocking pipe under load) that
        // returns partial writes. RedactingWriter::write must loop until
        // the full redacted record lands.
        struct OneByteAtATime {
            buf: Vec<u8>,
        }
        impl io::Write for OneByteAtATime {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if buf.is_empty() {
                    return Ok(0);
                }
                self.buf.push(buf[0]);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut writer = RedactingWriter {
            inner: OneByteAtATime { buf: Vec::new() },
            policy: Arc::new(RedactionPolicy::default()),
        };
        let input = b"INFO ptywright::run token=abc123def456 done\n";
        let n = writer.write(input).expect("write");
        assert_eq!(n, input.len(), "should report the full input as consumed");

        let text = String::from_utf8(writer.inner.buf).expect("utf8");
        assert!(text.contains("token=[REDACTED]"));
        assert!(text.contains("done\n"), "trailing bytes must not be lost");
        assert!(!text.contains("abc123def456"));
    }

    #[test]
    fn redacting_writer_redacts_secret_assignments() {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut sink,
                policy: Arc::new(RedactionPolicy::default()),
            };
            writer
                .write_all(b"INFO ptywright::run target=foo token=abc123def456 done\n")
                .expect("write");
            writer.flush().expect("flush");
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("token=[REDACTED]"),
            "expected redacted output, got: {text}"
        );
        assert!(!text.contains("abc123def456"));
    }

    #[test]
    fn make_filter_prefers_env_override() {
        // Acquire poison-tolerant: a panic in one env-mutating test must not
        // cascade into "lock poisoned" failures in the next.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os(ENV_FILTER);
        // SAFETY: lock above keeps env mutation single-threaded for this test.
        unsafe { std::env::set_var(ENV_FILTER, "trace") };

        let config = LoggingConfig {
            level: "warn".to_string(),
            ..LoggingConfig::default()
        };
        let filter = make_filter(&config);
        assert!(
            filter.to_string().contains("trace"),
            "PTYWRIGHT_LOG must override config level, got `{filter}`",
        );

        // SAFETY: still under the env lock.
        unsafe {
            match prev {
                Some(value) => std::env::set_var(ENV_FILTER, value),
                None => std::env::remove_var(ENV_FILTER),
            }
        }
    }

    #[test]
    fn make_filter_uses_config_level_when_env_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os(ENV_FILTER);
        // SAFETY: lock above serializes env mutation.
        unsafe { std::env::remove_var(ENV_FILTER) };

        let config = LoggingConfig {
            level: "info".to_string(),
            ..LoggingConfig::default()
        };
        let filter = make_filter(&config);
        assert!(
            filter.to_string().contains("info"),
            "config.level should drive filter, got `{filter}`",
        );

        // SAFETY: still under the env lock.
        unsafe {
            if let Some(v) = prev {
                std::env::set_var(ENV_FILTER, v);
            }
        }
    }

    #[test]
    fn make_filter_uses_default_when_config_level_blank() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os(ENV_FILTER);
        // SAFETY: lock above serializes env mutation.
        unsafe { std::env::remove_var(ENV_FILTER) };

        let config = LoggingConfig {
            level: "   ".to_string(),
            ..LoggingConfig::default()
        };
        let filter = make_filter(&config);
        assert!(
            filter.to_string().contains("warn"),
            "blank config level should fall back to `warn`, got `{filter}`",
        );

        // SAFETY: still under the env lock.
        unsafe {
            if let Some(v) = prev {
                std::env::set_var(ENV_FILTER, v);
            }
        }
    }

    #[test]
    fn ensure_log_dir_for_test_creates_directory() {
        let root = unique_tempdir("ensure-log-dir");
        let paths = Paths::with_root(&root);
        let dir = ensure_log_dir_for_test(&paths).expect("create logs dir");
        assert_eq!(dir, root.join("logs"));
        assert!(dir.exists());
        let _ = fs::remove_dir_all(root);
    }

    /// Set a file's mtime via the stable `std::fs::FileTimes` API. Used by
    /// the cleanup-age test to backdate a log file without pulling in the
    /// `filetime` crate; supported on every platform ptywright targets.
    fn filetime_set_mtime(path: &Path, target: SystemTime) -> io::Result<()> {
        let times = std::fs::FileTimes::new().set_modified(target);
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.set_times(times)?;
        Ok(())
    }

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
}
