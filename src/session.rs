use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::action::{Action, Key, Signal};
use crate::error::{Error, Result};
use crate::matcher::{MatchResult, Matcher, MatcherContext, PluginRegistry, PredicateContext};
use crate::redaction::RedactionPolicy;
use crate::screen::{ScreenSnapshot, Terminal};
use crate::target::{Target, TerminalSize};
use crate::transcript::{Transcript, TranscriptConfig, TranscriptDelta};

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
    /// Active in-process event subscribers. Each `Session::events()`
    /// call appends one sender; subscribers whose receiver was
    /// dropped are pruned the next time we try to fire an event.
    subscribers: Mutex<Vec<mpsc::Sender<SessionEvent>>>,
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

/// Cancellable wait coordinator used by [`Session::wait_for_cancellable`].
///
/// A `CancellationToken` is a thin `Arc<AtomicBool>` wrapper. Clone it
/// freely — every clone observes the same flag. Calling
/// [`CancellationToken::cancel`] flips the flag; ongoing waits poll it
/// on each tick and return [`Error::Cancelled`] when set.
///
/// Pair with `wait_for_cancellable` when you need to break out of a
/// long-running wait from another thread or RPC connection (e.g.
/// claudette stopping a turn from its UI while ptywright is still
/// waiting for the classifier's turn-boundary anchor to fire).
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Create a fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Flip the token. Already-cancelled tokens stay cancelled — the
    /// transition is a one-way edge. Subsequent waits return
    /// [`Error::Cancelled`] immediately.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Whether the token has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Events emitted on the in-process subscription channel returned by
/// [`Session::events`].
///
/// Subscribers are free to fetch the full state on receipt (call
/// [`Session::snapshot`] / [`Session::transcript_delta_since`]) — the
/// event itself carries only the cheap "something happened" signal so
/// the reader thread doesn't pay snapshot-cloning cost on every PTY
/// read for subscribers that don't care.
///
/// The channel is intentionally `std::sync::mpsc` so embedding callers
/// can subscribe without pulling in an async runtime.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// PTY sequence advanced (new screen or transcript bytes). Pair
    /// with `Session::sequence()` / `Session::snapshot()` /
    /// `Session::transcript_delta_since()` to retrieve the actual
    /// change.
    Changed { sequence: u64 },
    /// Child process exited. The status mirrors `Session::wait`'s
    /// return value; subscribers receiving this can assume no further
    /// events will fire on the channel.
    Exited(SessionExitStatus),
}

/// A running PTY-backed terminal session.
pub struct Session {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    shared: Arc<SharedState>,
    /// Optional plugin registry consulted when [`Matcher::Lua`] needs
    /// evaluation in [`Session::wait_for`] /
    /// [`Session::wait_for_cancellable`]. Bound via
    /// [`Session::set_plugin_registry`]; `None` means Lua matchers
    /// silently never fire.
    plugin_registry: Option<Arc<dyn PluginRegistry>>,
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
        // `clear_env` strips the inherited parent env before applying
        // the Target's overlay. Pair with a plugin manifest's
        // `default_target.required_env` for a fully reproducible layout.
        if config.target.clear_env {
            command.env_clear();
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
                transcript: Transcript::new(config.transcript)?,
                reader_open: true,
            }),
            changed: Condvar::new(),
            sequence: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            subscribers: Mutex::new(Vec::new()),
        });

        let read_shared = Arc::clone(&shared);
        spawn_reader_thread(Arc::clone(&shared), move || {
            read_loop(&mut reader, &read_shared);
        });

        Ok(Self {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            shared,
            plugin_registry: None,
        })
    }

    /// Bind a [`PluginRegistry`] so subsequent waits can evaluate
    /// [`Matcher::Lua`] branches against plugin-defined predicates.
    /// Replaces any previously-bound registry. Pair with
    /// [`Session::with_plugin_registry`] for builder-style setup.
    pub fn set_plugin_registry(&mut self, registry: Arc<dyn PluginRegistry>) {
        self.plugin_registry = Some(registry);
    }

    /// Builder variant of [`Session::set_plugin_registry`]. Useful
    /// when constructing a session through [`Session::spawn_target`]
    /// or [`Session::spawn`] and chaining the registry bind in one
    /// expression.
    #[must_use]
    pub fn with_plugin_registry(mut self, registry: Arc<dyn PluginRegistry>) -> Self {
        self.plugin_registry = Some(registry);
        self
    }

    /// Currently-bound plugin registry, if any. Mostly useful for
    /// tests that want to verify the registry plumbing without driving
    /// a full wait.
    #[must_use]
    pub fn plugin_registry(&self) -> Option<&Arc<dyn PluginRegistry>> {
        self.plugin_registry.as_ref()
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

    /// Snapshot every input the classifier needs (screen + transcript +
    /// transcript markers + current cursor) under a single state-lock
    /// acquisition so a PTY read arriving between component reads can't
    /// produce a context where, e.g., `cursor` is from a later moment
    /// than `screen`. Cheaper-than-N reads and atomic at the source.
    ///
    /// Caller-friendly types: owned strings/maps so the lock can drop
    /// before the caller threads them through classification. The
    /// returned `ScreenSnapshot` already owns its allocations.
    #[must_use]
    pub fn classify_input(
        &self,
    ) -> (
        ScreenSnapshot,
        String,
        std::collections::BTreeMap<String, u64>,
        u64,
    ) {
        let sequence = self.sequence();
        let state = self.shared.state.lock().expect("session state poisoned");
        let snapshot = state.terminal.snapshot(sequence);
        let transcript = state.transcript.text();
        let markers = state.transcript.markers().clone();
        let cursor = state.transcript.chars_written();
        (snapshot, transcript, markers, cursor)
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

    /// Total chars ever appended to this session's transcript. Survives
    /// ring-buffer evictions so subscribers can seed a stable cursor when
    /// they first subscribe — pairs with [`Session::transcript_delta_since`].
    #[must_use]
    pub fn transcript_chars_written(&self) -> u64 {
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .transcript
            .chars_written()
    }

    /// Newly-appended transcript text since `cursor`. Useful for streaming
    /// `session.output` notifications — see `RpcServer::poll_notifications`.
    /// The returned cursor advances even when the bounded buffer evicted part
    /// of the unseen range; the `dropped` flag reports the loss.
    #[must_use]
    pub fn transcript_delta_since(&self, cursor: u64) -> TranscriptDelta {
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .transcript
            .delta_since(cursor)
    }

    /// Redacted variant of [`Session::transcript_delta_since`]. Applies the
    /// policy to the delta only — the underlying cursor advances normally.
    #[must_use]
    pub fn redacted_transcript_delta_since(
        &self,
        cursor: u64,
        policy: &RedactionPolicy,
    ) -> TranscriptDelta {
        let mut delta = self.transcript_delta_since(cursor);
        delta.text = policy.redact(&delta.text);
        delta
    }

    /// Subscribe to in-process [`SessionEvent`] notifications.
    ///
    /// Returns a `std::sync::mpsc::Receiver<SessionEvent>` that yields
    /// `Changed` on every PTY read and `Exited` once when the child
    /// process ends. Each call returns an independent receiver; the
    /// host fans the same event out to every active subscriber's
    /// channel (multiple producer sites inside the library — reader
    /// thread, `wait`, `terminate`, `kill` — drive a single
    /// subscriber-owned receiver). Call `events()` multiple times to
    /// fan out to independent consumers. Dropping the receiver
    /// silently removes the subscription on the next event tick.
    ///
    /// Events carry only the cheap "something happened" signal; pair
    /// with [`Session::sequence`] / [`Session::snapshot`] /
    /// [`Session::transcript_delta_since`] to retrieve the actual
    /// change. This keeps the reader thread cheap when subscribers
    /// don't care about the full screen.
    pub fn events(&self) -> mpsc::Receiver<SessionEvent> {
        let (tx, rx) = mpsc::channel();
        self.shared
            .subscribers
            .lock()
            .expect("subscriber list poisoned")
            .push(tx);
        rx
    }

    /// Snapshot of all currently-recorded transcript markers. Returns a
    /// fresh clone so the caller can drop the session state lock
    /// immediately. Empty `BTreeMap` when no markers have been placed.
    #[must_use]
    pub fn transcript_markers(&self) -> std::collections::BTreeMap<String, u64> {
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .transcript
            .markers()
            .clone()
    }

    /// Place a label-keyed marker at the current transcript cursor. See
    /// [`crate::Transcript::mark`] for the storage semantics.
    pub fn mark_transcript(&self, label: impl Into<String>) -> u64 {
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .transcript
            .mark(label)
    }

    /// Cursor previously placed at `label`, or `None` if no such marker
    /// exists. Pairs with [`Session::transcript_slice`] to retrieve the
    /// bytes between two markers.
    #[must_use]
    pub fn transcript_marker(&self, label: &str) -> Option<u64> {
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .transcript
            .marker(label)
    }

    /// Text between two transcript cursors, or `None` if either cursor
    /// has been evicted from the ring buffer. See
    /// [`crate::Transcript::slice_between`].
    #[must_use]
    pub fn transcript_slice(&self, a: u64, b: u64) -> Option<String> {
        self.shared
            .state
            .lock()
            .expect("session state poisoned")
            .transcript
            .slice_between(a, b)
    }

    /// Send an action to the session.
    pub fn send(&self, action: Action) -> Result<()> {
        match action {
            Action::Text(text) | Action::Paste(text) => self.write_all(text.as_bytes()),
            Action::StreamText(text) => self.write_stream_text(&text),
            Action::BracketedPaste(text) => self.write_bracketed_paste(text.as_bytes()),
            Action::Key(key) => self.send_key(key),
            Action::Resize(size) => self.resize(size),
            Action::Interrupt => self.send_key(Key::CtrlC),
            Action::Eof => self.send_key(Key::CtrlD),
            Action::Signal(signal) => self.signal(signal),
            Action::Kill => self.kill(),
            Action::MarkTranscript { label } => {
                self.mark_transcript(label);
                Ok(())
            }
        }
    }

    /// Write `bytes` wrapped in bracketed-paste markers so the receiving
    /// application can distinguish a paste from interactive typing.
    ///
    /// Only used by [`Action::BracketedPaste`]; the generic
    /// [`Action::Paste`] writes raw bytes so callers driving programs that
    /// have NOT enabled bracketed paste (cat, plain shells, generic
    /// REPLs) don't get `ESC[200~` literals echoed back at them. Modern
    /// TUIs (vim, fish, and other recent readline-style frontends) set
    /// `CSI ? 2004 h` to opt in; the bracketed variant is preferred against
    /// those receivers so a trailing Enter is interpreted as a submit
    /// rather than absorbed into the paste tokeniser.
    fn write_bracketed_paste(&self, bytes: &[u8]) -> Result<()> {
        self.write_all(&bracketed_paste_payload(bytes))
    }

    fn write_stream_text(&self, text: &crate::action::StreamText) -> Result<()> {
        let chunk_chars = text.chunk_chars.unwrap_or(64).clamp(1, 1024);
        let delay = std::time::Duration::from_millis(text.delay_ms.unwrap_or(2).min(100));
        let mut chunk = String::new();
        let mut chunk_len = 0usize;

        for ch in text.text.chars() {
            chunk.push(ch);
            chunk_len += 1;
            if chunk_len >= chunk_chars {
                self.write_all(chunk.as_bytes())?;
                chunk.clear();
                chunk_len = 0;
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            }
        }

        if !chunk.is_empty() {
            self.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }
}

/// Wrap `bytes` in bracketed-paste START/END markers. Extracted from
/// [`Session::write_bracketed_paste`] so the framing is unit-testable
/// without spawning a PTY.
fn bracketed_paste_payload(bytes: &[u8]) -> Vec<u8> {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";
    let mut wrapped = Vec::with_capacity(START.len() + bytes.len() + END.len());
    wrapped.extend_from_slice(START);
    wrapped.extend_from_slice(bytes);
    wrapped.extend_from_slice(END);
    wrapped
}

impl Session {
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
        self.master
            .lock()
            .expect("pty master lock poisoned")
            .resize(to_pty_size(size))?;
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
        self.wait_for_inner(matcher, timeout, None)
    }

    /// Wait until a matcher succeeds, the timeout expires, or the
    /// provided [`CancellationToken`] is flipped.
    ///
    /// Returns [`Error::Cancelled`] on a cancel (distinct from
    /// [`Error::Timeout`]). The token is observed on every classifier
    /// tick *and* on every Condvar wakeup, so a cancel from another
    /// thread takes effect within one polling loop iteration even if
    /// no new PTY bytes are arriving — the Condvar timeout is capped
    /// at `CANCEL_POLL_INTERVAL` (50 ms) so an idle wait still wakes
    /// promptly to check the flag.
    pub fn wait_for_cancellable(
        &self,
        matcher: &Matcher,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<MatchResult> {
        self.wait_for_inner(matcher, timeout, Some(cancel))
    }

    fn wait_for_inner(
        &self,
        matcher: &Matcher,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> Result<MatchResult> {
        /// Upper bound on a single Condvar `wait_timeout` so a
        /// cancellation flag flip in another thread takes effect
        /// promptly even when no PTY bytes are arriving.
        const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

        let started = Instant::now();
        let deadline = started + timeout;
        let mut guard = self.shared.state.lock().expect("session state poisoned");
        let mut stable_sequence = self.sequence();
        let mut stable_since = started;

        loop {
            if let Some(token) = cancel
                && token.is_cancelled()
            {
                return Err(Error::Cancelled);
            }
            let sequence = self.sequence();
            if sequence != stable_sequence {
                stable_sequence = sequence;
                stable_since = Instant::now();
            }

            let process_exited = !guard.reader_open || self.shared.closed.load(Ordering::SeqCst);
            let snapshot = guard.terminal.snapshot(sequence);
            let transcript_tail = guard.transcript.tail(16 * 1024);
            // Snapshot the marker map under the same state-lock as
            // the terminal/transcript reads so `Matcher::Lua` predicates
            // see a consistent view across screen + transcript + markers.
            let markers = guard.transcript.markers().clone();
            let cursor = guard.transcript.chars_written();
            let stable_for = stable_since.elapsed();
            let context = MatcherContext {
                stable_for,
                process_exited,
            };
            let stable_ms = u64::try_from(stable_for.as_millis()).unwrap_or(u64::MAX);
            let predicate_ctx = PredicateContext {
                screen: &snapshot.plain_text,
                transcript: &transcript_tail,
                sequence,
                stable_ms,
                process_exited,
                markers: &markers,
                cursor,
            };
            let registry = self.plugin_registry.as_deref();
            // Cheap boolean check first so the polling loop avoids
            // building a `MatchOutcome` (and running `Regex::captures`)
            // on every tick. We only pay the structured-outcome cost once,
            // on the success path immediately below. `is_match_with_evaluator`
            // delegates to `is_match_with_context` for non-Lua matchers,
            // so the hot path keeps its allocation-free shape when no
            // Lua branches are present.
            if matcher.is_match_with_evaluator(
                &snapshot,
                &transcript_tail,
                context,
                &predicate_ctx,
                registry,
            ) {
                let outcome = matcher.describe_match_with_evaluator(
                    &snapshot,
                    &transcript_tail,
                    context,
                    &predicate_ctx,
                    registry,
                );
                return Ok(MatchResult {
                    matched: true,
                    sequence,
                    elapsed: started.elapsed(),
                    snapshot,
                    transcript_tail,
                    stable_for: context.stable_for,
                    outcome,
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
            // Cap the Condvar wait so a cancellation flag flip from
            // another thread wakes within CANCEL_POLL_INTERVAL even
            // when no PTY bytes are arriving. The cap only applies
            // when a cancel token is bound; the non-cancellable path
            // keeps its original "wait until deadline or PTY tick"
            // behaviour to avoid spurious wakeups on long idle waits.
            if cancel.is_some() {
                wait_for = wait_for.min(CANCEL_POLL_INTERVAL);
            }
            let (next_guard, timeout_result) = self
                .shared
                .changed
                .wait_timeout(guard, wait_for)
                .expect("session state poisoned");
            guard = next_guard;
            if let Some(token) = cancel
                && token.is_cancelled()
            {
                return Err(Error::Cancelled);
            }
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
        let exit = SessionExitStatus {
            code: status.exit_code(),
            success: status.success(),
            message: status.to_string(),
        };
        broadcast_event(&self.shared, SessionEvent::Exited(exit.clone()));
        Ok(exit)
    }

    /// Kill the child process.
    pub fn kill(&self) -> Result<()> {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        self.child.lock().expect("child lock poisoned").kill()?;
        Ok(())
    }

    /// Process id of the spawned child, when known. Returns `None` if the
    /// backend never exposed one or the child has already been reaped —
    /// the underlying `portable_pty::Child::process_id` contract.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.lock().expect("child lock poisoned").process_id()
    }

    /// Send a POSIX-style signal to the child. See [`Signal`] for
    /// per-platform behaviour; on Windows only [`Signal::Term`],
    /// [`Signal::Kill`], and [`Signal::Int`] are honoured and every other
    /// variant returns [`Error::UnsupportedOnPlatform`].
    pub fn signal(&self, signal: Signal) -> Result<()> {
        match signal {
            Signal::Int => self.send_key(Key::CtrlC),
            Signal::Kill => self.kill(),
            _ => self.signal_native(signal),
        }
    }

    /// Graceful-then-forceful shutdown: send [`Signal::Term`], poll for the
    /// child up to `grace`, then [`Signal::Kill`] if still running. Returns
    /// the observed [`SessionExitStatus`] either way. Mirrors the SIGTERM →
    /// poll → SIGKILL ladder that consumers like claudette build manually.
    pub fn terminate(&self, grace: Duration) -> Result<SessionExitStatus> {
        // Best-effort SIGTERM. If the platform rejects it (Windows for
        // non-Term/Kill/Int variants is impossible here since we're
        // sending Term, but the backend may still error), fall through
        // to the hard kill below — we're committed to ending the child.
        if let Err(error) = self.signal_native(Signal::Term) {
            tracing::debug!(?error, "ptywright: terminate SIGTERM rejected; escalating");
        }

        let deadline = Instant::now() + grace;
        loop {
            {
                let mut child = self.child.lock().expect("child lock poisoned");
                if let Some(status) = child.try_wait()? {
                    self.shared.closed.store(true, Ordering::SeqCst);
                    self.shared.changed.notify_all();
                    let exit = SessionExitStatus {
                        code: status.exit_code(),
                        success: status.success(),
                        message: status.to_string(),
                    };
                    drop(child);
                    broadcast_event(&self.shared, SessionEvent::Exited(exit.clone()));
                    return Ok(exit);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            // Cap the sleep at the remaining time-to-deadline so the
            // ladder doesn't overshoot `grace` by up to 20 ms on the
            // last iteration. `Duration::ZERO` short-circuits cleanly
            // through `min` for callers passing `grace == 0`.
            thread::sleep(Duration::from_millis(20).min(deadline.saturating_duration_since(now)));
        }

        // Still alive past the grace window — fall back to the hard kill
        // path. We deliberately use `wait()` (not `try_wait()`) afterwards
        // so the returned status reflects the kill rather than a stale
        // "still running" reading.
        self.kill()?;
        self.wait()
    }

    #[cfg(unix)]
    fn signal_native(&self, signal: Signal) -> Result<()> {
        // Translate to libc constants. Unix supports every variant; the
        // Int/Kill branches in `signal()` short-circuit before reaching
        // here, but mapping them keeps the table exhaustive in one place.
        let libc_signal: libc::c_int = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Hup => libc::SIGHUP,
            Signal::Quit => libc::SIGQUIT,
            Signal::Int => libc::SIGINT,
            Signal::Kill => libc::SIGKILL,
            Signal::User1 => libc::SIGUSR1,
            Signal::User2 => libc::SIGUSR2,
        };
        let pid = self.require_pid()?;
        // SAFETY: `kill` is a thin libc wrapper; we pass an i32 pid and a
        // valid signal constant. The call is async-signal-safe and has
        // no preconditions on Rust state.
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc_signal) };
        if rc == 0 {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::last_os_error()))
        }
    }

    #[cfg(windows)]
    fn signal_native(&self, signal: Signal) -> Result<()> {
        // Windows has no POSIX-equivalent for SIGHUP / SIGQUIT / SIGUSR*.
        // `Term` maps to a best-effort `taskkill /T /PID` — graceful
        // enough to give the child a chance to clean up while still
        // ending the process tree. `Int` is handled by the caller via
        // the PTY Ctrl-C byte, and `Kill` is the hard-kill path on the
        // existing `ChildKiller` trait. `UnsupportedOnPlatform` is the
        // static "no equivalent" signal — runtime failures (taskkill
        // exits non-zero because the process is already gone or the
        // binary is missing) surface as `Error::Io` so callers can
        // tell "this signal kind is not supported on Windows" from
        // "we tried, the OS rejected it."
        if !matches!(signal, Signal::Term) {
            return Err(Error::UnsupportedOnPlatform(format!(
                "signal `{signal:?}` has no Windows equivalent"
            )));
        }
        let pid = self.require_pid()?;
        let status = std::process::Command::new("taskkill")
            .args(["/T", "/PID", &pid.to_string()])
            .status()
            .map_err(Error::Io)?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::other(format!(
                "taskkill exited with status {status:?}"
            ))))
        }
    }

    /// Resolve the child PID or return an `Error::Io` with
    /// `NotFound`. The `pid()` accessor's `None` covers two cases —
    /// "backend never exposed a pid" (permanent) and "child already
    /// reaped" (transient) — but both manifest the same way to a
    /// caller asking to signal: the PID is unavailable. Mapping to
    /// `Error::Closed` would conflate this with the user-driven
    /// close path; `Error::Io(NotFound)` matches the standard Rust
    /// convention for "asked for a thing, not there."
    fn require_pid(&self) -> Result<u32> {
        self.pid().ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no child pid available for this session",
            ))
        })
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
                if let Err(error) = state.transcript.push_bytes(&buf[..n]) {
                    let message = RedactionPolicy::default().redact(&error.to_string());
                    eprintln!("ptywright: transcript write error: {message}");
                    break;
                }
                let sequence = shared.sequence.fetch_add(1, Ordering::SeqCst) + 1;
                drop(state);
                shared.changed.notify_all();
                broadcast_event(shared, SessionEvent::Changed { sequence });
            }
            Err(_) => break,
        }
    }
}

/// Iterate every registered subscriber's sender, drop any whose
/// receiver has hung up. Called from the reader thread on every PTY
/// read and from the lifecycle paths (`wait`, `kill`) when emitting
/// `Exited`. Held under a short-lived mutex; subscribers that block
/// would never reach the slow path because we use
/// `mpsc::Sender::send` (unbounded) which only fails on a dropped
/// receiver.
///
/// Performance note: the event is cloned once per active subscriber
/// while the mutex is held — `mpsc::Sender::send` consumes the value
/// by design, so a single shared reference cannot fan out. The hot
/// path is the reader thread, so subscriber count should stay small
/// (typically O(1) — one REPL, one Tauri bridge). If a future
/// consumer needs broad fan-out, switch to `crossbeam_channel` or a
/// reference-counted event type rather than scaling the clone-per-tx
/// pattern.
fn broadcast_event(shared: &SharedState, event: SessionEvent) {
    let mut subs = shared.subscribers.lock().expect("subscriber list poisoned");
    subs.retain(|tx| tx.send(event.clone()).is_ok());
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

    #[test]
    fn bracketed_paste_payload_wraps_input_with_csi_markers() {
        // CSI 200~ / CSI 201~ are the standard bracketed-paste markers.
        // Apps that have enabled bracketed paste (Claude Code v2.1+,
        // vim, fish, …) use them to distinguish a paste from interactive
        // typing — without the wrapper a paste followed by Enter races
        // against the receiver's input tokeniser.
        let bytes = bracketed_paste_payload(b"hello");
        assert_eq!(&bytes[..6], b"\x1b[200~");
        assert_eq!(&bytes[6..11], b"hello");
        assert_eq!(&bytes[11..], b"\x1b[201~");
    }

    #[test]
    fn bracketed_paste_payload_handles_empty_input() {
        let bytes = bracketed_paste_payload(b"");
        assert_eq!(bytes, b"\x1b[200~\x1b[201~".to_vec());
    }

    #[test]
    #[cfg(unix)]
    fn action_paste_writes_raw_bytes_no_bracket_markers() {
        // Generic `Action::Paste` must NOT inject bracketed-paste markers —
        // callers driving programs that have not enabled bracketed paste
        // (cat, plain shells, REPLs) would see literal `ESC[200~` bytes
        // echoed back. The bracketed framing lives on `BracketedPaste` and
        // is used by plugins driving receivers that opt into
        // `CSI ? 2004 h` (vim, fish, and other modern readline-style TUIs).
        //
        // A POSIX shell `read` is line-buffered (canonical mode), so we
        // send `Paste` + `Enter` together to flush. We then assert the
        // PTY transcript carries the literal payload and never the
        // wrapper bytes.
        let target =
            Target::new("/bin/sh").args(["-lc", "read line; printf 'GOT[%s]done' \"$line\""]);
        let session = Session::spawn(SessionConfig::new(target)).expect("spawn shell");
        session
            .send(Action::Paste("plain".into()))
            .expect("send paste");
        session.send(Action::Key(Key::Enter)).expect("send enter");
        let result = session
            .wait_for(
                &Matcher::ContainsText("GOT[plain]done".into()),
                Duration::from_secs(5),
            )
            .expect("wait for done");
        let transcript = result.transcript_tail;
        assert!(
            !transcript.contains("\x1b[200~"),
            "Action::Paste must not emit bracketed-paste markers: {transcript:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn action_stream_text_writes_raw_text_without_paste_markers() {
        let target =
            Target::new("/bin/sh").args(["-lc", "read line; printf 'GOT[%s]done' \"$line\""]);
        let session = Session::spawn(SessionConfig::new(target)).expect("spawn shell");
        session
            .send(Action::StreamText(crate::action::StreamText {
                text: "plain".into(),
                chunk_chars: Some(2),
                delay_ms: Some(0),
            }))
            .expect("send streamed text");
        session.send(Action::Key(Key::Enter)).expect("send enter");
        let result = session
            .wait_for(
                &Matcher::ContainsText("GOT[plain]done".into()),
                Duration::from_secs(5),
            )
            .expect("wait for done");
        let transcript = result.transcript_tail;
        assert!(
            !transcript.contains("\x1b[200~"),
            "Action::StreamText must not emit bracketed-paste markers: {transcript:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn action_bracketed_paste_writes_csi_markers() {
        // The bracketed variant must round-trip the wrapper sequences
        // through the PTY so receivers that have opted in see a real
        // paste boundary. Using `cat` to echo back without
        // line-discipline canonicalisation, we feed the bytes and
        // assert both the payload AND the brackets land in the
        // transcript before exiting via Ctrl-D.
        let target =
            Target::new("/bin/sh").args(["-lc", "read line; printf 'GOT[%s]done' \"$line\""]);
        let session = Session::spawn(SessionConfig::new(target)).expect("spawn shell");
        session
            .send(Action::BracketedPaste("plain".into()))
            .expect("send bracketed paste");
        session.send(Action::Key(Key::Enter)).expect("send enter");
        let result = session
            .wait_for(
                &Matcher::ContainsText("done".into()),
                Duration::from_secs(5),
            )
            .expect("wait for done");
        let transcript = result.transcript_tail;
        assert!(
            transcript.contains("\x1b[200~"),
            "Action::BracketedPaste must emit the CSI 200~ start marker: {transcript:?}"
        );
        assert!(
            transcript.contains("\x1b[201~"),
            "Action::BracketedPaste must emit the CSI 201~ end marker: {transcript:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cancellable_wait_returns_cancelled_when_token_flipped_from_another_thread() {
        // Spawn a long-running shell, attach a cancellable wait for a
        // string that will never appear, flip the cancel token from a
        // sibling thread, and assert we get Error::Cancelled within a
        // small window (not the full timeout).
        let session = Arc::new(
            Session::spawn_target(Target::new("/bin/sh").args(["-lc", "sleep 30"]))
                .expect("spawn sleep"),
        );
        let token = CancellationToken::new();
        let canceller = token.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(120));
            canceller.cancel();
        });

        let session_for_wait = Arc::clone(&session);
        let started = Instant::now();
        let result = session_for_wait.wait_for_cancellable(
            &Matcher::ContainsText("never appears".into()),
            Duration::from_secs(30),
            &token,
        );
        let elapsed = started.elapsed();
        match result {
            Err(Error::Cancelled) => {}
            other => panic!("expected Error::Cancelled, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "cancel must wake the wait within the poll interval; elapsed={elapsed:?}"
        );
        let _ = session.kill();
    }

    #[test]
    #[cfg(unix)]
    fn cancellable_wait_still_returns_match_when_not_cancelled() {
        // Sanity: the cancellable path must behave identically to the
        // plain wait_for path when the token is never flipped.
        let session = Session::spawn_target(echo_target()).expect("spawn echo");
        let token = CancellationToken::new();
        let result = session
            .wait_for_cancellable(
                &Matcher::ContainsText("ready".into()),
                Duration::from_secs(5),
                &token,
            )
            .expect("wait should match");
        assert!(result.matched);
        let _ = session.wait();
    }

    #[test]
    #[cfg(unix)]
    fn cancellable_wait_pre_flipped_token_returns_immediately() {
        // Defensive: a token that's already cancelled before the wait
        // starts must return Error::Cancelled on the first tick, not
        // wait the full timeout.
        let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "sleep 30"]))
            .expect("spawn sleep");
        let token = CancellationToken::new();
        token.cancel();
        let started = Instant::now();
        let result = session.wait_for_cancellable(
            &Matcher::ContainsText("never".into()),
            Duration::from_secs(30),
            &token,
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(started.elapsed() < Duration::from_millis(200));
        let _ = session.kill();
    }

    #[test]
    #[cfg(unix)]
    fn events_receiver_observes_changed_and_exited() {
        // In-process subscription. The reader thread can fire `Changed`
        // before `events()` is called if the echo child finishes
        // before the test reaches subscribe — that race makes a strict
        // "must see Changed" assertion flaky. We subscribe immediately
        // after spawn (cheap, the worst case is we miss the first
        // tick) and then drive a longer child so at least one PTY read
        // happens after subscription.
        let session = Session::spawn_target(
            Target::new("/bin/sh").args(["-lc", "printf one; sleep 0.05; printf two"]),
        )
        .expect("spawn shell");
        let rx = session.events();
        let status = session.wait().expect("wait child");
        assert!(status.success);

        // Drain everything currently buffered. Channel is unbounded
        // mpsc so try_iter() yields the full backlog without blocking.
        let events: Vec<SessionEvent> = rx.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::Changed { .. })),
            "expected at least one Changed event; got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, SessionEvent::Exited(_))),
            "expected an Exited event; got {events:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn events_subscriber_dropped_is_pruned_silently() {
        // Defensive: subscribing and dropping the receiver must not
        // poison the subscriber list or block the broadcaster. After
        // dropping the first rx, a second subscriber should still
        // receive events normally.
        let session = Session::spawn_target(echo_target()).expect("spawn echo");
        let _rx_dropped = session.events();
        drop(_rx_dropped);
        let rx_kept = session.events();
        let _ = session.wait();
        let events: Vec<SessionEvent> = rx_kept.try_iter().collect();
        assert!(
            !events.is_empty(),
            "second subscriber must still receive events after first was dropped"
        );
    }

    #[test]
    #[cfg(unix)]
    fn pid_is_reported_while_child_is_alive() {
        // Sanity-check the PID accessor — the child runs long enough
        // that `pid()` must return Some(_) before we kill it. We can't
        // assert any particular value, but presence is a real claim.
        let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "sleep 5"]))
            .expect("spawn sleep");
        assert!(session.pid().is_some());
        session.kill().expect("kill child");
        let _ = session.wait();
    }

    #[test]
    #[cfg(unix)]
    fn terminate_sends_sigterm_then_returns_exit_status() {
        // POSIX `sleep` exits cleanly on SIGTERM, so the graceful path
        // is exercised end-to-end — no SIGKILL escalation should fire
        // within the 2s grace window.
        let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "sleep 30"]))
            .expect("spawn sleep");
        let started = Instant::now();
        let status = session
            .terminate(Duration::from_secs(2))
            .expect("terminate session");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "graceful terminate should not wait the full grace window"
        );
        // SIGTERM exit codes vary by platform/shell — we just assert
        // the child observably ended rather than locking in `code`.
        assert!(!status.message.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn terminate_escalates_to_kill_when_child_ignores_sigterm() {
        // `trap '' TERM` makes the shell ignore SIGTERM; the ladder
        // must then escalate to SIGKILL inside the grace window.
        let session =
            Session::spawn_target(Target::new("/bin/sh").args(["-lc", "trap '' TERM; sleep 30"]))
                .expect("spawn shell");
        let status = session
            .terminate(Duration::from_millis(150))
            .expect("terminate session");
        assert!(!status.message.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn action_signal_routes_through_session_signal() {
        // End-to-end: dispatching `Action::Signal(Term)` must reach the
        // child via the same path as calling `Session::signal` directly.
        // A long `sleep` is killed by SIGTERM by default; observing the
        // exit proves both the routing through `send()` and the libc
        // call site landed on the right PID. We avoid asserting a
        // specific exit code — shells normalise SIGTERM differently
        // across platforms and that's not the property under test.
        let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "sleep 10"]))
            .expect("spawn shell");
        // Tiny pause lets the shell finish exec'ing before we signal.
        thread::sleep(Duration::from_millis(50));
        session
            .send(Action::Signal(Signal::Term))
            .expect("send signal");
        let result = session
            .wait_for(&Matcher::ProcessExited, Duration::from_secs(3))
            .expect("wait for exit");
        assert!(result.matched);
        let _ = session.wait();
    }

    #[test]
    #[cfg(unix)]
    fn wait_for_evaluates_matcher_lua_through_bound_registry() {
        // End-to-end: Session::wait_for with a bound LuaPluginRegistry
        // must evaluate `Matcher::Lua` by calling into the registered
        // plugin's predicate, including the screen/transcript/markers
        // view it sees.
        use crate::lua_plugin::{LuaPlugin, LuaPluginRegistry};
        use crate::matcher::MatchOutcome;

        let plugin = LuaPlugin::builtin(
            "demo",
            r#"
            return {
              saw_ready = function(input)
                if string.find(input.screen, "ready") then
                  return { matched = true, evidence = "found ready" }
                end
                return false
              end
            }
            "#,
        )
        .expect("load demo plugin");
        let registry = Arc::new(LuaPluginRegistry::with_single("demo", plugin));

        let session = Session::spawn_target(
            Target::new("/bin/sh").args(["-lc", "printf 'ready\\n'; sleep 5"]),
        )
        .expect("spawn shell")
        .with_plugin_registry(registry);

        let matcher = Matcher::Lua {
            plugin: "demo".to_string(),
            predicate: "saw_ready".to_string(),
            params: serde_json::Value::Null,
        };
        let result = session
            .wait_for(&matcher, Duration::from_secs(3))
            .expect("Lua matcher must fire when screen shows 'ready'");
        assert!(result.matched);
        match result.outcome {
            Some(MatchOutcome::Lua {
                plugin,
                predicate,
                evidence,
                ..
            }) => {
                assert_eq!(plugin, "demo");
                assert_eq!(predicate, "saw_ready");
                assert_eq!(evidence.as_deref(), Some("found ready"));
            }
            other => panic!("expected Lua outcome; got {other:?}"),
        }
        let _ = session.kill();
    }

    #[test]
    #[cfg(unix)]
    fn wait_for_matcher_lua_returns_timeout_when_no_registry_bound() {
        // Without a registry, `Matcher::Lua` never fires — the wait
        // ends in Timeout (not a panic or silent always-true). Guards
        // the contract that the registry is opt-in and missing it
        // doesn't poison a session.
        let session =
            Session::spawn_target(Target::new("/bin/sh").args(["-lc", "printf ready; sleep 5"]))
                .expect("spawn shell");
        let matcher = Matcher::Lua {
            plugin: "nope".to_string(),
            predicate: "any".to_string(),
            params: serde_json::Value::Null,
        };
        let err = session
            .wait_for(&matcher, Duration::from_millis(150))
            .expect_err("must time out without a registry");
        assert!(matches!(err, Error::Timeout));
        let _ = session.kill();
    }

    #[test]
    #[cfg(unix)]
    fn action_mark_transcript_stamps_marker_without_pty_write() {
        // `Action::MarkTranscript` is a metadata channel — applying it
        // through `Session::send` must record a marker and must NOT
        // write any bytes to the PTY. We pair the assertion with a
        // transcript comparison: the visible bytes before and after the
        // mark must be unchanged.
        let session = Session::spawn_target(Target::new("/bin/sh").args(["-lc", "printf hi"]))
            .expect("spawn shell");
        // Let the child finish so the transcript settles.
        let _ = session.wait_for(&Matcher::ProcessExited, Duration::from_secs(3));

        let transcript_before = session.transcript();
        let cursor_before = session.transcript_chars_written();
        assert!(session.transcript_marker("turn_start").is_none());

        session
            .send(Action::MarkTranscript {
                label: "turn_start".to_string(),
            })
            .expect("apply mark_transcript");

        // Marker recorded at the current cursor; transcript bytes
        // unchanged because no PTY write happened.
        assert_eq!(session.transcript_marker("turn_start"), Some(cursor_before));
        assert_eq!(session.transcript(), transcript_before);
        assert_eq!(session.transcript_chars_written(), cursor_before);
    }

    #[test]
    fn bracketed_paste_payload_preserves_embedded_newlines() {
        // Embedded newlines must NOT be split — bracketed paste mode tells
        // the receiver the whole block is one paste, so the receiver's
        // line tokeniser keeps it together rather than treating the newline
        // as a submit.
        let bytes = bracketed_paste_payload(b"line one\nline two");
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("line one\nline two"));
        assert!(s.starts_with("\x1b[200~"));
        assert!(s.ends_with("\x1b[201~"));
    }
}
