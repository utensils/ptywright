use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::action::{Action, Key};
use crate::error::{Error, Result};
use crate::matcher::{MatchResult, Matcher, MatcherContext};
use crate::redaction::RedactionPolicy;
use crate::screen::{ScreenSnapshot, Terminal};
use crate::target::{Target, TerminalSize};
use crate::transcript::{Transcript, TranscriptConfig};

/// Configuration for a PTY-backed session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Program target to spawn.
    pub target: Target,
    /// Transcript retention configuration.
    pub transcript: TranscriptConfig,
}

impl SessionConfig {
    /// Create a config for a target with default transcript retention.
    #[must_use]
    pub fn new(target: Target) -> Self {
        Self {
            target,
            transcript: TranscriptConfig::default(),
        }
    }
}

struct SessionState {
    terminal: Terminal,
    transcript: Transcript,
    reader_open: bool,
}

struct SharedState {
    state: Mutex<SessionState>,
    changed: Condvar,
    sequence: AtomicU64,
    closed: AtomicBool,
}

/// Exit status for a completed session child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExitStatus {
    /// Process exit code normalized by the PTY backend.
    pub code: u32,
    /// Whether the process exited successfully.
    pub success: bool,
    /// Human-readable status text from the backend.
    pub message: String,
}

/// A running PTY-backed terminal session.
pub struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    shared: Arc<SharedState>,
}

impl Session {
    /// Spawn a new PTY-backed session.
    pub fn spawn(config: SessionConfig) -> Result<Self> {
        let size = config.target.size;
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(to_pty_size(size))?;

        let mut command = CommandBuilder::new(&config.target.program);
        command.args(config.target.args.iter().map(String::as_str));
        if let Some(cwd) = &config.target.cwd {
            command.cwd(cwd.as_os_str());
        }
        for (key, value) in &config.target.env {
            command.env(key, value);
        }

        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let shared = Arc::new(SharedState {
            state: Mutex::new(SessionState {
                terminal: Terminal::new(size),
                transcript: Transcript::new(config.transcript),
                reader_open: true,
            }),
            changed: Condvar::new(),
            sequence: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });

        let read_shared = Arc::clone(&shared);
        spawn_reader_thread(Arc::clone(&shared), move || {
            read_loop(&mut reader, &read_shared);
        });

        Ok(Self {
            master: pair.master,
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            shared,
        })
    }

    /// Convenience constructor with default transcript retention.
    pub fn spawn_target(target: Target) -> Result<Self> {
        Self::spawn(SessionConfig::new(target))
    }

    /// Current session output sequence number.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.shared.sequence.load(Ordering::SeqCst)
    }

    /// Return a rendered screen snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ScreenSnapshot {
        let sequence = self.sequence();
        let state = self.shared.state.lock().expect("session state poisoned");
        state.terminal.snapshot(sequence)
    }

    /// Whether the PTY reader or explicit lifecycle state indicates the session has finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
            || !self
                .shared
                .state
                .lock()
                .expect("session state poisoned")
                .reader_open
    }

    /// Return the retained transcript text.
    #[must_use]
    pub fn transcript(&self) -> String {
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .transcript
            .text()
    }

    /// Return the retained transcript text with sensitive-looking values redacted.
    #[must_use]
    pub fn redacted_transcript(&self, policy: &RedactionPolicy) -> String {
        policy.redact(&self.transcript())
    }

    /// Send an action to the session.
    pub fn send(&self, action: Action) -> Result<()> {
        match action {
            Action::Text(text) | Action::Paste(text) => self.write_all(text.as_bytes()),
            Action::Key(key) => self.send_key(key),
            Action::Resize(size) => self.resize(size),
            Action::Interrupt => self.send_key(Key::CtrlC),
            Action::Eof => self.send_key(Key::CtrlD),
            Action::Kill => self.kill(),
        }
    }

    /// Write raw text bytes to the PTY master.
    pub fn write_text(&self, text: impl AsRef<str>) -> Result<()> {
        self.write_all(text.as_ref().as_bytes())
    }

    /// Send a named key to the PTY master.
    pub fn send_key(&self, key: Key) -> Result<()> {
        self.write_all(key.bytes())
    }

    /// Resize the PTY and terminal parser.
    pub fn resize(&self, size: TerminalSize) -> Result<()> {
        self.master.resize(to_pty_size(size))?;
        let mut state = self.shared.state.lock().expect("session state poisoned");
        state.terminal.resize(size);
        let next = self.shared.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        drop(state);
        self.shared.changed.notify_all();
        let _ = next;
        Ok(())
    }

    /// Wait until a matcher succeeds or the timeout expires.
    pub fn wait_for(&self, matcher: &Matcher, timeout: Duration) -> Result<MatchResult> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut guard = self.shared.state.lock().expect("session state poisoned");
        let mut stable_sequence = self.sequence();
        let mut stable_since = started;

        loop {
            let sequence = self.sequence();
            if sequence != stable_sequence {
                stable_sequence = sequence;
                stable_since = Instant::now();
            }

            let process_exited = !guard.reader_open || self.shared.closed.load(Ordering::SeqCst);
            let snapshot = guard.terminal.snapshot(sequence);
            let transcript_tail = guard.transcript.tail(16 * 1024);
            let context = MatcherContext {
                stable_for: stable_since.elapsed(),
                process_exited,
            };
            if matcher.is_match_with_context(&snapshot, &transcript_tail, context) {
                return Ok(MatchResult {
                    matched: true,
                    sequence,
                    elapsed: started.elapsed(),
                    snapshot,
                    transcript_tail,
                });
            }

            if process_exited && matcher.minimum_stable_duration().is_none() {
                return Err(Error::Closed);
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout);
            }
            let mut wait_for = deadline.saturating_duration_since(now);
            if let Some(min_stable) = matcher.minimum_stable_duration() {
                let stable_for = stable_since.elapsed();
                if stable_for < min_stable {
                    wait_for = wait_for.min(min_stable - stable_for);
                }
            }
            let (next_guard, timeout_result) = self
                .shared
                .changed
                .wait_timeout(guard, wait_for)
                .expect("session state poisoned");
            guard = next_guard;
            if timeout_result.timed_out() && Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
        }
    }

    /// Wait for the child process to exit.
    pub fn wait(&self) -> Result<SessionExitStatus> {
        let status = self.child.lock().expect("child lock poisoned").wait()?;
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        Ok(SessionExitStatus {
            code: status.exit_code(),
            success: status.success(),
            message: status.to_string(),
        })
    }

    /// Kill the child process.
    pub fn kill(&self) -> Result<()> {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        self.child.lock().expect("child lock poisoned").kill()?;
        Ok(())
    }

    fn write_all(&self, bytes: &[u8]) -> Result<()> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        let mut writer = self.writer.lock().expect("writer lock poisoned");
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }
}

fn spawn_reader_thread<F>(shared: Arc<SharedState>, f: F)
where
    F: FnOnce() + Send + 'static,
{
    thread::spawn(move || {
        f();
        let mut state = shared.state.lock().expect("session state poisoned");
        state.reader_open = false;
        shared.changed.notify_all();
    });
}

fn read_loop(reader: &mut Box<dyn Read + Send>, shared: &SharedState) {
    let mut buf = [0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut state = shared.state.lock().expect("session state poisoned");
                state.terminal.process(&buf[..n]);
                state.transcript.push_bytes(&buf[..n]);
                shared.sequence.fetch_add(1, Ordering::SeqCst);
                drop(state);
                shared.changed.notify_all();
            }
            Err(_) => break,
        }
    }
}

fn to_pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn echo_target() -> Target {
        Target::new("/bin/sh").args(["-lc", "printf ready"])
    }

    #[cfg(windows)]
    fn echo_target() -> Target {
        Target::new("cmd.exe").args(["/C", "echo ready"])
    }

    #[test]
    #[cfg(unix)]
    fn session_captures_process_output() {
        let session = Session::spawn_target(echo_target()).expect("spawn session");

        let result = session
            .wait_for(
                &Matcher::ContainsText("ready".into()),
                Duration::from_secs(5),
            )
            .expect("wait for ready");

        assert!(result.snapshot.plain_text.contains("ready"));
        assert!(session.transcript().contains("ready"));
        assert!(session.wait().expect("wait child").success);
    }

    #[test]
    #[cfg(unix)]
    fn wait_for_screen_stable() {
        let session =
            Session::spawn_target(Target::new("/bin/sh").args(["-lc", "printf ready; sleep 0.2"]))
                .expect("spawn session");

        let result = session
            .wait_for(
                &Matcher::ScreenStable { min_ms: 50 },
                Duration::from_secs(5),
            )
            .expect("wait for stable screen");

        assert!(result.elapsed >= Duration::from_millis(50));
        assert!(session.wait().expect("wait child").success);
    }

    #[test]
    #[cfg(unix)]
    fn wait_for_process_exited() {
        let session = Session::spawn_target(echo_target()).expect("spawn session");

        let result = session
            .wait_for(&Matcher::ProcessExited, Duration::from_secs(5))
            .expect("wait for process exit");

        assert!(result.snapshot.plain_text.contains("ready"));
        assert!(session.wait().expect("wait child").success);
    }
}
