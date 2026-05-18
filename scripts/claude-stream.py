#!/usr/bin/env python3
"""claude-stream — drive Claude Code through ptywright and stream the result.

Usage:
  claude-stream "your prompt"
  claude-stream @path/to/prompt.md
  claude-stream -                # read prompt from stdin
  claude-stream --help

Spawns `ptywright serve --stdio`, starts the built-in `claude-code` adapter,
auto-handles workspace-trust and any first-keypress interceptors, submits
the prompt with verification, streams the response, and exits cleanly on
completion or SIGINT.

Design goals:
  * **Robust across Claude Code TUI versions.** All decisions use observable
    screen evidence (body changed / didn't change / contains text X) plus
    the classifier's state label — never version-specific glyphs or
    spinner verbs.
  * **No magic timings.** Two knobs only: `--heartbeat-ms` (the streaming
    cadence) and `--timeout` (the overall safety bound). Every wait is
    event-driven (poll until X happens) with the overall timeout as the
    single fallback. There are no hand-tuned "wait 0.5s" sleeps anywhere
    in the control flow.
  * **Native-feel streaming.** At the default 50 ms heartbeat the body
    deltas appear within ~25 ms of leaving the PTY.
  * **Clean Ctrl+C.** SIGINT closes the adapter and the ptywright server
    in order; the parent shell does not see a partial pipe or a zombie.

Requires `claude` on PATH and `ptywright` either on PATH or via
$PTYWRIGHT_BIN / the devshell wrapper.
"""
from __future__ import annotations

import argparse
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

# ───────────────────────── terminal styling ─────────────────────────────────

USE_COLOR = sys.stdout.isatty() and os.environ.get("NO_COLOR") is None
def c(code: str, text: str) -> str:
    if not USE_COLOR: return text
    return f"\033[{code}m{text}\033[0m"
DIM     = lambda s: c("2", s)
BOLD    = lambda s: c("1", s)
CYAN    = lambda s: c("36", s)
GREEN   = lambda s: c("32", s)
YELLOW  = lambda s: c("33", s)
RED     = lambda s: c("31", s)
MAGENTA = lambda s: c("35", s)

def emit(prefix: str, body: str = ""):
    if sys.stdout.isatty():
        sys.stdout.write("\r\033[K")
    sys.stdout.write(f"{prefix} {body}\n" if body else f"{prefix}\n")
    sys.stdout.flush()

# ───────────────────────── arg parsing ──────────────────────────────────────

def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        prog="claude-stream",
        description="Drive Claude Code via ptywright and stream the response.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    ap.add_argument("prompt", help='prompt string, @path/to/file, or - for stdin')
    ap.add_argument("--cwd", default=os.getcwd(), help="cwd for the claude session (default: $PWD)")
    ap.add_argument("--model", default="sonnet",
                    help="model passed to `claude --model` (default: sonnet; pass --model '' to let claude choose). Haiku tends to acknowledge-and-stop without using tools on broad prompts, which makes the demo look like the stream exited early; sonnet engages with tool-using prompts more reliably.")
    ap.add_argument("--timeout", type=float, default=600.0,
                    help="overall hard safety bound in seconds (default: 600). The script never "
                         "spins past this regardless of internal state — it is the only time-based "
                         "guard in the control flow.")
    ap.add_argument("--heartbeat-ms", type=int, default=50,
                    help="poll cadence in ms (default: 50). Pulses session.output notifications "
                         "and screen inspections; lower = faster perceived streaming, higher CPU.")
    ap.add_argument("--stuck-after-seconds", type=float, default=60.0,
                    help="while the classifier reports `thinking`, bail if no meaningful "
                         "PTY output (>=256 bytes — see PASTE_REACTION_BYTES) has been "
                         "observed for this many seconds (default: 60). Catches silent "
                         "hangs where the model never produces a first token (network "
                         "stall, account auth refresh) — robust to status-bar token-"
                         "counter trickle that would otherwise reset a naive timer. "
                         "The default leaves headroom for Claude to surface its own "
                         "rate-limit / quota error banner (observed up to ~90s on the "
                         "1M-context tier) so wrappers get exit 3 / 4 instead of exit "
                         "5 when an account is throttled. Set to 0 to disable.")
    ap.add_argument("--ptywright", default=os.environ.get("PTYWRIGHT_BIN", "ptywright"),
                    help="path to the ptywright binary (default: $PTYWRIGHT_BIN or `ptywright`)")
    ap.add_argument("--quiet", action="store_true", help="suppress state-transition and metadata annotations")
    return ap.parse_args()

def resolve_prompt(arg: str) -> str:
    if arg == "-":
        return sys.stdin.read()
    if arg.startswith("@"):
        return Path(arg[1:]).read_text(encoding="utf-8")
    return arg

# ───────────────────────── jsonrpc client ───────────────────────────────────

class Client:
    """NDJSON JSON-RPC client over a ptywright stdio subprocess.

    Background reader routes id-bearing responses to per-call queues and
    drops notifications into a thread-safe queue. Synchronous rpc() calls
    are the only path the main thread uses.
    """
    def __init__(self, bin_path: str, log_level: str):
        self.proc = subprocess.Popen(
            [bin_path, "serve", "--stdio"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
            env={**os.environ, "PTYWRIGHT_LOG": log_level},
        )
        self.responses: dict[int, queue.Queue] = {}
        self.notifs: queue.Queue = queue.Queue()
        self.lock = threading.Lock()
        self._rid = 0
        self._dead = False
        threading.Thread(target=self._reader, daemon=True).start()
        threading.Thread(target=self._stderr_reader, daemon=True).start()

    def _reader(self):
        try:
            for line in self.proc.stdout:
                line = line.strip()
                if not line: continue
                try: m = json.loads(line)
                except json.JSONDecodeError: continue
                if "id" in m:
                    with self.lock:
                        q = self.responses.get(m["id"])
                    if q: q.put(m)
                else:
                    self.notifs.put(m)
        finally:
            self._dead = True

    def _stderr_reader(self):
        for line in self.proc.stderr:
            sys.stderr.write(f"{DIM('[ptywright]')} {line}")

    def rpc(self, method: str, params: dict | None = None, t: float = 10.0):
        if self._dead:
            raise RuntimeError("ptywright server has exited")
        self._rid += 1
        rid = self._rid
        q: queue.Queue = queue.Queue()
        with self.lock: self.responses[rid] = q
        req: dict = {"jsonrpc":"2.0","id":rid,"method":method}
        if params: req["params"] = params
        try:
            self.proc.stdin.write(json.dumps(req) + "\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError) as e:
            # Subprocess closed stdin / was terminated between the _dead
            # check above and this write. Surface a clean RuntimeError so
            # the stream loop's existing exception handler returns 1
            # instead of leaking a traceback. Common race: SIGINT handler
            # calls client.proc.terminate() while the main thread is mid-rpc.
            self._dead = True
            with self.lock: self.responses.pop(rid, None)
            raise RuntimeError(f"ptywright server has exited (write failed: {e})") from e
        try:
            msg = q.get(timeout=t)
        except queue.Empty:
            raise TimeoutError(f"{method} did not respond within {t}s") from None
        finally:
            with self.lock: self.responses.pop(rid, None)
        if "error" in msg:
            raise RuntimeError(f"{method} failed: {msg['error']}")
        return msg["result"]

    def drain_notifs(self) -> list[dict]:
        out: list[dict] = []
        try:
            while True: out.append(self.notifs.get_nowait())
        except queue.Empty: pass
        return out

    def close(self):
        try: self.proc.stdin.close()
        except Exception: pass
        try: self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            try: self.proc.wait(timeout=2)
            except subprocess.TimeoutExpired: pass

# ───────────────────────── streaming driver ─────────────────────────────────

# Single bytes-of-PTY-growth threshold used to verify Claude has reacted
# to a paste. Derived from observed PTY behaviour, not arbitrary:
#   * Claude's idle status-bar refresh produces ~120 bytes per heartbeat.
#   * A real prompt acceptance (echoing the input, rendering the spinner)
#     produces 1000+ bytes within the first heartbeat post-paste.
# 256 bytes sits squarely between the two: above idle noise, well below
# any real reaction. It's a byte threshold, not a timing — it doesn't
# get stale across faster/slower machines because it measures Claude's
# *output bytes*, not wall-clock.
PASTE_REACTION_BYTES = 256

# Structural filters for body chrome — lines that exist purely as TUI
# decoration and should not be streamed as content. Matching is by
# shape, not by glyph identity, so any TUI that uses similar chrome
# patterns gets filtered without listing every Unicode variant.
#
#   * `_CHROME_RULE_RE` — horizontal rules made of one repeated
#     box-drawing or em-dash character (Claude Code uses `─`).
#   * `_CHROME_SPINNER_RE` — spinner status lines: a leading glyph
#     FROM THE KNOWN SPINNER SET, then a verb ending with the
#     horizontal ellipsis `…`, optionally followed by a parenthesized
#     counter section like `(9s · ↑ 217 tokens · thinking)` or
#     `(ctrl+o to expand)` that mutates every frame. The completion
#     line (`✻ <Verb> for <N>`) does NOT end with an ellipsis and
#     contains ` for <digit>`, so it intentionally doesn't match —
#     it's the real end-of-turn signal we want to surface. Requiring
#     the glyph to be in the spinner set (not `\S+`) prevents
#     legitimate one-sentence prose like `"Done… (for now)"` from
#     matching: ordinary words aren't in the spinner glyph set.
#   * `_CHROME_SEARCHED_RE` — IN-FLIGHT progress lines only:
#     `Searching for 1 pattern… (ctrl+o to expand)` style. The
#     ellipsis is required, which keeps the COMPLETED forms
#     (`Searched for 1 pattern (ctrl+o to expand)`, `Read 3 files
#     (ctrl+o to expand)`) flowing through as content — those are the
#     informative tool-call summaries the user wants to see. The
#     in-flight form mutates every tick (count, verb) and is what
#     would flood the output without filtering.
#
# `_CHROME_SPINNER_GLYPHS` MUST stay in sync with `SPINNER_GLYPHS` in
# `plugins/claude-code/indicators` (currently `main.lua`). If a new
# spinner glyph appears in a Claude Code release, add it to both.
_CHROME_SPINNER_GLYPHS = "✶✻✺✦·•⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⏺✽✳✢⏵"
_CHROME_RULE_RE = re.compile(r"^[─━═]+$")
_CHROME_SPINNER_RE = re.compile(
    r"^[" + _CHROME_SPINNER_GLYPHS + r"]\s+\S.*…(?:\s*\([^)]*\))?$"
)
_CHROME_SEARCHED_RE = re.compile(
    r"^(?:Searching|Reading|Listing)\b.*…\s*\(ctrl\+o to expand\)$"
)
# Activity-extraction pattern for the alive ticker — same shape as
# `_CHROME_SPINNER_RE` plus the bullet-form tool announcements
# (`⏺ Read(file)`, `⏺ Bash(...)`). The ticker shows the most recent
# such line so the user can tell what Claude is doing between tool
# completions, even while the in-flight form itself is filtered out
# of the streaming output.
_ACTIVITY_TOOL_BULLET_RE = re.compile(
    r"^⏺\s+\S+\(.+\)\s*$"
)

# Turn-completion marker recognizer for the streaming diff. Same
# shape `has_turn_completion_marker` in the plugin uses: leading `✻`
# glyph + tea-verb + ` for <N>` numeric duration. Distinguishes the
# real end-of-turn marker from spinner frames (which end in `…`).
# When we see this in a streamed line, we KNOW Claude has finished
# the turn even if the classifier hasn't yet reported `completed_turn`
# — the stuck-detector's rescue branch uses this fact.
_COMPLETION_MARKER_RE = re.compile(r"^✻\s+\S+.*\sfor\s+\d")

def _is_chrome_line(s: str) -> bool:
    if _CHROME_RULE_RE.match(s):
        return True
    if _CHROME_SPINNER_RE.match(s) and len(s) <= 120:
        return True
    if _CHROME_SEARCHED_RE.match(s):
        return True
    return False


def _extract_recent_activity(body: str) -> str | None:
    """Return the most recent line in `body` that looks like a tool
    call in progress or just completed. Used by the alive ticker so
    the user can tell what Claude is doing between tool completions —
    the in-flight spinner / progress lines are filtered out of the
    streaming output, but their content is still useful as a status
    indicator.

    Walks bottom-up because the spinner sits at the bottom of the
    body, just above the status bar. Skips horizontal rules and
    long-tail wrapped lines. Returns `None` if nothing recognisable
    is on screen.
    """
    for line in reversed(body.splitlines()):
        s = line.strip()
        if not s:
            continue
        if _CHROME_RULE_RE.match(s):
            continue
        if _CHROME_SPINNER_RE.match(s) and len(s) <= 120:
            return s
        if _CHROME_SEARCHED_RE.match(s):
            return s
        if _ACTIVITY_TOOL_BULLET_RE.match(s):
            return s
    return None

class Stream:
    """Robust Claude Code streaming driver — TUI-version-agnostic."""

    SUCCESS_STATE = "completed_turn"
    # Any of these end the stream. Only SUCCESS_STATE is a clean exit;
    # `error` / `exited` / `plugin_error` are failure terminations and
    # must return a non-zero exit code so CI / callers don't treat
    # them as success.
    TERMINAL_STATES = {"completed_turn", "error", "exited", "plugin_error"}

    # Exit-code taxonomy. Wrappers (claudette, multi-account drivers,
    # CI) discriminate failure modes on these. 3 / 4 are reserved for
    # the two plan-level conditions where swapping accounts is the
    # natural remediation; everything else collapses to 1.
    EXIT_OK = 0
    EXIT_GENERIC = 1
    EXIT_RATE_LIMIT = 3
    EXIT_QUOTA = 4
    EXIT_STUCK = 5

    def __init__(self, client: Client, aid: str, args: argparse.Namespace,
                 hard_deadline: float):
        self.client = client
        self.aid = aid
        self.args = args
        self.heartbeat = args.heartbeat_ms / 1000.0
        self.deadline = hard_deadline
        self.stuck_after = max(0.0, getattr(args, "stuck_after_seconds", 60.0))
        self._closed = False
        # Sticky flag set when the server emits `session.exited` for this
        # adapter's underlying session. The poll loop checks it BEFORE
        # the classifier state so a Claude crash mid-stream surfaces
        # immediately rather than spinning until the hard deadline. The
        # classifier itself never returns the literal state string
        # `exited` (it's a host-side concept), so this flag is how the
        # terminal-state guard in `stream_until_done` learns about it.
        self._session_exited = False
        # Annotation memo — only print transitions, not every poll.
        self._last_state: str | None = None
        self._last_evidence: str | None = None
        self._last_metadata_repr: str | None = None
        # Most-recent classifier metadata dict. Captured every poll so
        # the terminal-state branch can read `metadata.error.kind`
        # (rate_limit / quota / connection / auth / api / unknown) and
        # pick the right exit code without re-RPC'ing.
        self._last_metadata: dict | None = None
        # Stuck-detector state. Independent of the TTY-only KB/s ticker
        # so it works under non-TTY stdout (CI piping). Sampled at
        # most once per second to keep RPC overhead low.
        self._growth_check_at: float = 0.0
        self._growth_last_len: int | None = None
        self._growth_last_grew_at: float | None = None
        # Latched True the first time the streaming diff emits a line
        # matching `_COMPLETION_MARKER_RE`. When the stuck-detector
        # subsequently fires, we know Claude actually finished the
        # turn — the classifier just got tripped by post-turn tips /
        # spinner repaints. We exit cleanly as `completed_turn`
        # instead of `stuck` in that case.
        self._saw_completion_marker: bool = False
        # Alive ticker — overwrites in place on a TTY.
        self._last_alive_at = 0.0
        self._alive_phase = 0
        # Transcript length at the moment the prompt was acknowledged.
        # Set by `submit()`. Used by the completion-time answer dump so
        # we can scan only the content this turn added to the scrollback
        # — the transcript is the canonical record of everything Claude
        # rendered, even rows that scrolled past the visible body. The
        # visible body is a 60×200 alt-screen snapshot; on a long answer
        # the early tool-call output rotates out of it before the
        # classifier fires `completed_turn`, but the transcript still has
        # every byte.
        self._transcript_baseline: int = 0
        # Submitted prompt text — used as an anchor in the transcript
        # answer-region scan.
        self._submitted_prompt: str = ""
        # Transcript bytes/sec tracking — used by the alive ticker to
        # surface "Claude is alive even though the body is frozen"
        # during `Explore` subagent runs where the main TUI doesn't
        # repaint for tens of seconds but the PTY is still receiving
        # raw bytes (status-bar token counter updates, etc.). Sampled
        # roughly every second from the ticker.
        self._bytes_rate_last_t = 0.0
        self._bytes_rate_last_len = 0
        self._bytes_rate_kbps = 0.0

    # ─── time discipline ─────────────────────────────────────────────────
    def _budget(self) -> float:
        """Seconds remaining before the hard deadline fires."""
        return max(0.0, self.deadline - time.monotonic())

    def _expired(self) -> bool:
        return time.monotonic() >= self.deadline

    # ─── primitive: read what's on screen now ────────────────────────────
    def inspect(self) -> tuple[str, dict]:
        ins = self.client.rpc("adapter.inspect", {"adapter": self.aid, "redact": False}, t=5.0)
        return ins.get("body_text", ""), ins

    # ─── primitive: full screen (body + status) for activity scans ───────
    def _screen_for_activity(self, ins: dict) -> str:
        """Concatenate body + status for the activity ticker.

        Claude Code 2.1.x typically renders the spinner row at the bottom
        of the body region, but during `Explore` subagent runs the only
        visible motion is in the status bar (token counter, "Thinking…"
        line, etc.). The body diff still only prints body lines, but the
        ticker should reflect *all* activity, so we feed it the union.
        """
        body = ins.get("body_text", "") or ""
        status = ins.get("status_text", "") or ""
        if not status:
            return body
        return body + "\n" + status

    # ─── primitive: poll state once, annotate transitions ────────────────
    def poll_state(self) -> dict:
        st = self.client.rpc("adapter.state", {"adapter": self.aid}, t=5.0)
        s = st["state"]["state"]
        ev = st["state"].get("evidence", "")
        md = st["state"].get("metadata")
        self._last_metadata = md if isinstance(md, dict) else None
        if not self.args.quiet:
            # Only log on actual state transitions. Evidence can flap
            # within a single state (e.g. the mid-turn `thinking` branch
            # alternates between "active work indicator detected" and
            # "turn in flight; no accepted completion marker on screen" as the
            # spinner repaints), and logging each evidence change made
            # the output look like the classifier was flapping when it
            # was steady. The post-transition evidence is preserved on
            # the line so the reader still sees the most recent reason.
            if s != self._last_state:
                emit(MAGENTA("⇢ state"),
                     f"{self._last_state} → {BOLD(s)}  {DIM(ev)}" if self._last_state
                     else f"{BOLD(s)}  {DIM(ev)}")
                self._last_state = s
                self._last_evidence = ev
            if md:
                md_repr = json.dumps(md, sort_keys=True)
                if md_repr != self._last_metadata_repr:
                    if "permission" in md:
                        p = md["permission"]
                        emit(YELLOW("⚠ permission"),
                             f"tool={p.get('tool','?')} summary={(p.get('summary') or '')[:80]}")
                    if "status" in md:
                        s_md = md["status"]
                        emit(DIM("  status"),
                             f"model={s_md.get('model','?')} mode={s_md.get('permission_mode','?')}")
                    if "usage" in md:
                        u = md["usage"]
                        emit(GREEN("$ usage"),
                             " ".join(f"{k}={v}" for k, v in u.items() if v is not None))
                    self._last_metadata_repr = md_repr
        return st["state"]

    # ─── primitive: wait for the server's notion of "screen settled" ─────
    def wait_for_settled(self, max_wait_s: float | None = None) -> None:
        """Block until the server reports the PTY screen has been stable.

        Uses the server-side `screen_stable` matcher rather than client-side
        polling. The stability window is whatever the server is configured
        with (`completed_turn_stable_ms` in ~/.ptywright/config.toml; default
        300 ms). This means the script and the plugin's own classifier agree
        on what "stable" means — there is no client-side timing decision to
        keep in sync with the server.

        Bounded by the overall hard deadline, or by `max_wait_s` when the
        caller wants this to be a best-effort settle before trying a
        separately verified action.
        """
        budget = self._budget()
        if max_wait_s is not None:
            budget = min(budget, max_wait_s)
        budget_ms = int(budget * 1000)
        if budget_ms <= 0: return
        try:
            self.client.rpc("adapter.wait", {
                "adapter": self.aid,
                "intent": "wait_turn_matcher",
                # Server reads `completed_turn_stable_ms` from config and
                # injects it as `min_ms` into the underlying screen_stable
                # matcher — no client-side timing magic.
                "params": {},
                "timeout_ms": budget_ms,
            }, t=budget + 5.0)
        except (RuntimeError, TimeoutError):
            # Wait timed out or matcher failed; that's OK — the caller's
            # next step (which always has its own verification path) will
            # surface the real outcome.
            pass

    # ─── primitive: liveness indicator that doesn't spam ─────────────────
    def _tick_alive(self, prefix: str = "", body: str = ""):
        if not sys.stdout.isatty(): return
        now = time.monotonic()
        # Throttle to at most one tick per 4 heartbeats so the spinner
        # cadence is independent of how aggressively we poll.
        if now - self._last_alive_at < self.heartbeat * 4: return
        self._last_alive_at = now
        self._alive_phase = (self._alive_phase + 1) % 8
        glyph = "⠋⠙⠹⠸⠼⠴⠦⠧"[self._alive_phase]
        elapsed = self.args.timeout - self._budget()
        # Surface what Claude is currently doing — extracted from the
        # body (typically the spinner / tool-call announcement at the
        # bottom). Truncated to keep the alive line on one row even on
        # narrow terminals.
        activity = _extract_recent_activity(body) if body else None
        if activity and len(activity) > 80:
            activity = activity[:77] + "…"
        # Transcript bytes/sec — only refresh ~once per second so the
        # number doesn't jitter at the heartbeat cadence. Surfaces
        # "Claude is alive" even when the main TUI body is frozen
        # (subagent runs, extended-thinking phases that only repaint
        # the status-bar token counter).
        rate_str = self._sample_bytes_rate()
        bits: list[str] = []
        if activity:
            bits.append(activity)
        if rate_str:
            bits.append(rate_str)
        suffix = ("  " + "  ".join(bits)) if bits else ""
        sys.stdout.write(f"\r{DIM(f'{glyph} {prefix} [{elapsed:.0f}s]{suffix}')}\033[K")
        sys.stdout.flush()

    def _sample_bytes_rate(self) -> str:
        """Return a `+N.N KB/s` indicator of recent transcript growth,
        or "" if we don't have enough samples yet. Refreshes only once
        per second so the number doesn't jitter at heartbeat cadence."""
        now = time.monotonic()
        if self._bytes_rate_last_t and now - self._bytes_rate_last_t < 1.0:
            # Reuse the cached rate without polling — keeps the ticker
            # snappy and the cached value visible between samples.
            return self._format_kbps(self._bytes_rate_kbps)
        try:
            trans = self.client.rpc("adapter.transcript",
                {"adapter": self.aid, "redact": False}, t=2.0)["text"]
        except (RuntimeError, TimeoutError):
            return ""
        cur_len = len(trans)
        if self._bytes_rate_last_t:
            dt = max(now - self._bytes_rate_last_t, 0.001)
            dbytes = max(cur_len - self._bytes_rate_last_len, 0)
            self._bytes_rate_kbps = (dbytes / 1024.0) / dt
        self._bytes_rate_last_t = now
        self._bytes_rate_last_len = cur_len
        return self._format_kbps(self._bytes_rate_kbps)

    @staticmethod
    def _format_kbps(kbps: float) -> str:
        # Show the zero-rate case explicitly (`idle`) rather than empty
        # string — when Sonnet 4.6 spawns an Explore subagent and prep-
        # thinks for tens of seconds, the PTY produces nothing and the
        # rate is genuinely 0. Without this branch the ticker shows
        # only `thinking [Ns]` with no suffix, which looks identical to
        # a hung process. Surfacing `idle` makes it clear that the
        # sample ran and confirms no PTY activity.
        if kbps <= 0:
            return "idle"
        if kbps < 1.0:
            return f"+{int(kbps * 1024)} B/s"
        return f"+{kbps:.1f} KB/s"

    def _clear_alive(self):
        if sys.stdout.isatty():
            sys.stdout.write("\r\033[K")
            sys.stdout.flush()

    # ─── fallback: dump answer region from final body ────────────────────
    def _dump_answer_region(self, body: str) -> None:
        """Print the answer region from the final body snapshot.

        Called at completion when the streaming loop didn't surface any
        content of its own. Common scenarios:
          * Model gave a very brief reply (the entire response fits in
            the gap between two polling ticks).
          * Content was rewritten in place faster than the diff caught.

        The "answer region" is everything from after the FIRST `❯ …`
        prompt-with-text line (the user's submitted prompt echo, at the
        top of the body) up to (but not including) the first trailing
        anchor we recognise — either an empty `❯` idle prompt or a
        post-turn ghost-text suggestion (`❯ run the tests` after the
        completion marker). Anchoring `start` on the FIRST prompt-with-
        text instead of the LAST is critical: Claude Code 2.1.x renders
        a post-turn ghost-text suggestion using the SAME `❯ <text>`
        shape, and if we used the last occurrence we'd skip over the
        actual answer to the suggestion at the bottom and print
        nothing. The submitted prompt always lives at the top of the
        visible body in Claude Code's layout, so the first occurrence
        is the reliable anchor.

        Chrome lines (horizontal rules, spinner status) inside the
        answer region are skipped using the same structural filter
        the streaming loop uses.
        """
        lines = body.splitlines()

        def _is_prompt_with_text(s: str) -> bool:
            # Restrict to `❯` (Claude Code 2.1.x's actual input-box
            # glyph). Earlier this also accepted `> <text>`, but ASCII
            # `>` is also the Markdown blockquote glyph — Claude's
            # answer prose containing `> some quoted text` would be
            # misidentified as a prompt-with-text and the answer-region
            # scan would terminate prematurely. Bare `>` (no text) is
            # still treated as an IDLE prompt by `_is_idle_prompt`
            # below; that's safe because Markdown blockquotes always
            # have text after the `>`.
            if not s.startswith("❯"):
                return False
            tail = s[1:].lstrip("  ")
            if not tail:
                return False
            # Numbered selections like `❯ 1.` are dialog focus glyphs,
            # not prompt text.
            if tail[:1].isdigit() and len(tail) > 1 and tail[1] in ".):":
                return False
            return True

        def _is_idle_prompt(s: str) -> bool:
            # Either bare glyph or glyph + only NBSP padding.
            return s in ("❯", ">") or s.replace(" ", "") in ("❯", ">")

        # Find the FIRST prompt-with-text line — the user's submitted
        # prompt echo at the top of the body.
        start = 0
        for i, line in enumerate(lines):
            s = line.strip()
            if _is_prompt_with_text(s):
                start = i + 1
                break

        # Find the FIRST trailing prompt row after `start` — either
        # the idle prompt (`❯` alone) or a post-turn ghost suggestion
        # (`❯ run the tests`). Both mark the end of the answer region.
        end = len(lines)
        for i in range(start, len(lines)):
            s = lines[i].strip()
            if _is_idle_prompt(s) or _is_prompt_with_text(s):
                end = i
                break

        printed = False
        for line in lines[start:end]:
            s = line.strip()
            if not s:
                continue
            if _is_chrome_line(s):
                continue
            if not printed:
                self._clear_alive()
                printed = True
            sys.stdout.write(line + "\n")
        if printed:
            sys.stdout.flush()

    # ─── fallback: dump answer region from transcript scrollback ────────
    def _dump_answer_region_from_transcript(
        self, already_printed: set[str] | None = None
    ) -> bool:
        """Print the answer region using the full transcript scrollback.

        The visible body is a single alt-screen snapshot (60×200 cells
        by default). On a long answer the early tool-call output and
        even Claude's prose can scroll OUT of the visible body before
        the classifier fires `completed_turn` — at that point the body
        only shows the last few rows plus the marker plus the idle
        prompt, and `_dump_answer_region(body)` prints nothing because
        there are no content lines between its anchors.

        The transcript captures every PTY byte that landed since
        `_transcript_baseline` was set (right before `send_prompt`).
        We scan from there, find the LAST submitted-prompt echo
        (`❯ <prompt text>`), find the FIRST completion marker after
        that, and print everything between, filtering chrome the same
        way the streaming loop does.

        Returns True if anything was printed, False otherwise.
        """
        try:
            trans = self.client.rpc("adapter.transcript",
                {"adapter": self.aid, "redact": False}, t=5.0)["text"]
        except (RuntimeError, TimeoutError):
            return False
        # Only scan content this turn added to the scrollback — pre-turn
        # noise (welcome banner, status-bar refreshes, prior turns) lives
        # before `_transcript_baseline`.
        delta = trans[self._transcript_baseline:]
        if not delta.strip():
            return False

        lines = delta.splitlines()

        # Anchor on the LAST `❯` prompt-row that contains the submitted
        # prompt text. Two anchors combined: the line must have the
        # prompt-echo SHAPE (`❯ <text>` or `❯<NBSP><text>`) AND contain
        # the submitted prompt's first line as a substring. Shape alone
        # would match the post-turn ghost suggestion; substring alone
        # could match Claude's answer if it quotes or repeats the prompt
        # text in prose. Both together pin the submitted-prompt echo.
        # If only the substring matches (no shape match) we fall back
        # to substring-only at LAST occurrence as a last resort.
        anchor = self._submitted_prompt.strip().splitlines()[0] if self._submitted_prompt.strip() else ""
        start = 0
        if anchor:
            shape_match = -1
            substring_match = -1
            for i in range(len(lines) - 1, -1, -1):
                line = lines[i]
                if anchor in line:
                    if substring_match < 0:
                        substring_match = i
                    stripped = line.lstrip()
                    if stripped.startswith("❯ ") or stripped.startswith("❯ "):
                        shape_match = i
                        break
            if shape_match >= 0:
                start = shape_match + 1
            elif substring_match >= 0:
                start = substring_match + 1

        # End at the FIRST line after `start` that looks like the
        # completion marker (`✻ <Verb> for <N>` with no ellipsis tail).
        # If we don't find one, dump everything to end-of-delta.
        end = len(lines)
        for i in range(start, len(lines)):
            s = lines[i].strip()
            if s.startswith("✻") and " for " in s and not s.endswith("…"):
                end = i + 1
                break

        printed = False
        # Dedup so we don't double-print intermediate frames the
        # transcript captured. Seed with what the streaming loop has
        # already printed (when called as a gap-filler at completion),
        # so any line the diff path emitted isn't repeated here.
        seen: set[str] = set(already_printed) if already_printed else set()
        for line in lines[start:end]:
            s = line.strip()
            if not s:
                continue
            if _is_chrome_line(s):
                continue
            # Skip the post-turn ghost suggestion line shape — it sits
            # below the marker, but in transcript order it can appear
            # interleaved with the answer if Claude redraws. Restrict
            # to `❯` (the actual prompt glyph) so legitimate Markdown
            # blockquotes (`> some quoted text`) in Claude's answer
            # aren't silently dropped. Handle both regular-space and
            # NBSP (U+00A0) padding — Claude Code 2.1.x renders ghost
            # text with NBSP rather than regular space.
            if s.startswith("❯ ") or s.startswith("❯ "):
                continue
            if s in seen:
                continue
            seen.add(s)
            if not printed:
                self._clear_alive()
                printed = True
            sys.stdout.write(line + "\n")
        if printed:
            sys.stdout.flush()
        return printed

    # ─── action: send a single named key ─────────────────────────────────
    def send_key(self, key: str):
        self.client.rpc("adapter.send",
                        {"adapter": self.aid, "intent": "key", "params": {"key": key}}, t=5.0)

    # ─── action: paste a prompt (bracketed paste + Enter) ────────────────
    def send_prompt(self, prompt: str):
        self.client.rpc("adapter.send",
                        {"adapter": self.aid, "intent": "send_prompt",
                         "params": {"prompt": prompt}}, t=10.0)

    # ─── compound: submit prompt and verify it landed ────────────────────
    def submit(self, prompt: str, input_prompt_already_observed: bool = False) -> tuple[bool, str]:
        """Submit the prompt. The plugin's `send_prompt` intent now emits
        Enter → BracketedPaste → Enter, which both dismisses any
        first-keypress interceptor (welcome panel, compact-launch view)
        AND submits the prompt. So the script's responsibility shrinks
        to: wait until the screen is stable, then send_prompt.

        `input_prompt_already_observed=True` skips the initial settle
        wait: `wait_turn_matcher` only wakes for end-of-turn anchors
        (completion marker / dialogs / usage), not for the idle input
        prompt by itself. If the startup loop already saw
        `waiting_for_user_input` we know the input box is rendered and
        ready, so blocking for `max_wait_s` here only adds a fixed
        delay before every submission. The submit-verification loop
        below has its own paste-acknowledgement check, which is the
        real correctness guard.

        Verification: poll the transcript for growth above noise. A real
        paste acceptance produces 1000+ bytes of PTY output within the
        first heartbeat; silent absorption produces only idle status-bar
        noise (~120 bytes per tick). PASTE_REACTION_BYTES sits between.

        Returns (success, failure_reason).
        """
        if input_prompt_already_observed:
            emit(DIM("· input box already observed; skipping settle wait"))
        else:
            emit(DIM("· waiting for Claude's input box to settle"))
            self.wait_for_settled(max_wait_s=3.0)
        if self._expired(): return False, "deadline expired before initial settle"

        baseline = len(self.client.rpc("adapter.transcript",
            {"adapter": self.aid, "redact": False}, t=5.0)["text"])
        # Remember the baseline + prompt so `stream_until_done` can
        # extract this turn's answer region from the transcript even if
        # it scrolled past the visible body.
        self._transcript_baseline = baseline
        self._submitted_prompt = prompt

        # Use a short, distinctive substring of the prompt as a body-
        # anchor. After a real submission, the prompt is echoed in
        # the body as `❯ <prompt text>`. A failed submission leaves
        # it ONLY in `status_text` (the input box at the bottom),
        # so checking for the snippet inside `body_text` is a much
        # stronger signal than "body bytes changed" (which is
        # trivially true between any two inspects).
        prompt_anchor = prompt.strip().splitlines()[0] if prompt.strip() else ""
        prompt_anchor = prompt_anchor[:40]  # head only, in case of long prompts

        def _do_send():
            self.send_prompt(prompt)
            grew = 0
            sub_deadline = time.monotonic() + 10.0
            while time.monotonic() < sub_deadline and not self._expired():
                trans = self.client.rpc("adapter.transcript",
                    {"adapter": self.aid, "redact": False}, t=5.0)["text"]
                grew = len(trans) - baseline
                cur_body, _ = self.inspect()
                # Real submission echoes the prompt into BODY (not
                # just status_text). Without this check, the launch
                # banner's idle redraws pass a naive body-diff check
                # even when nothing was submitted.
                anchored = bool(prompt_anchor) and (prompt_anchor in cur_body)
                if grew >= PASTE_REACTION_BYTES and anchored:
                    return True, grew
                self._tick_alive(f"waiting for Claude to accept prompt (+{grew}B)")
                time.sleep(self.heartbeat)
            return False, grew

        ok, grew = _do_send()
        if not ok:
            # First attempt didn't get past the launch / interceptor
            # state. Send an extra Enter via the plugin's
            # `dismiss_welcome` intent (single Enter, no paste) to
            # clear whatever's intercepting, then retry the prompt.
            emit(DIM("· first submit didn't land — retrying after dismiss_welcome"))
            try:
                self.client.rpc("adapter.send",
                    {"adapter": self.aid, "intent": "dismiss_welcome", "params": {}}, t=5.0)
            except (RuntimeError, TimeoutError):
                pass
            ok, grew = _do_send()
        if ok:
            emit(GREEN("→ prompt submitted"),
                 DIM(f"({len(prompt)} chars; +{grew} bytes from Claude)"))
            return True, ""

        while not self._expired():
            # Fall-through path — keep polling until either anchor
            # lands or the hard deadline fires.
            trans = self.client.rpc("adapter.transcript",
                {"adapter": self.aid, "redact": False}, t=5.0)["text"]
            grew = len(trans) - baseline
            cur_body, _ = self.inspect()
            anchored = bool(prompt_anchor) and (prompt_anchor in cur_body)
            if grew >= PASTE_REACTION_BYTES and anchored:
                emit(GREEN("→ prompt submitted"),
                     DIM(f"({len(prompt)} chars; +{grew} bytes from Claude)"))
                return True, ""
            self._tick_alive(f"waiting for Claude to acknowledge paste (+{grew}B)")
            time.sleep(self.heartbeat)
        # Deadline expired while the paste was in flight. The send_prompt
        # actions already landed in the PTY, so if Claude is just slow we
        # don't want to leave a half-submitted turn running server-side
        # after we return. Best-effort `cancel` (sends Escape — see
        # `M.cancel` in the Lua plugin) before the caller closes the
        # adapter. Wrapped because the server might already be unhealthy
        # when this fires; failure here is informational, not fatal.
        try:
            self.client.rpc("adapter.send",
                {"adapter": self.aid, "intent": "cancel", "params": {}}, t=2.0)
        except (RuntimeError, TimeoutError):
            pass
        return False, "deadline expired waiting for Claude to acknowledge paste"

    # ─── primitive: idle-growth sampling for the stuck detector ──────────
    def _check_idle_growth(self) -> float | None:
        """Return seconds since the last observed MEANINGFUL transcript
        growth, or None when we don't have enough samples yet. Samples
        `adapter.transcript` at most once per second to keep RPC
        overhead negligible relative to the heartbeat loop.

        "Meaningful" here is growth of at least `PASTE_REACTION_BYTES`
        (256) since the last reset point — the same noise floor the
        submit-verification path uses. Below that the trickle is just
        status-bar chrome (token counter, spinner, idle refresh, dim
        elapsed timer) which keeps repainting during a genuine hang
        and would otherwise pin a naive timer at zero forever.
        `_growth_last_len` is deliberately NOT updated for sub-
        threshold growth, so trickle accumulates across samples — a
        real 256-byte burst spread across N seconds will still reset
        the timer; only true silence (or pure status-bar repainting)
        ticks it forward.

        Independent of `_sample_bytes_rate` (which is TTY-display-only,
        cached behind the alive ticker's gating). This path runs even
        under non-TTY stdout so CI piping behaves the same as a live
        terminal.
        """
        now = time.monotonic()
        # Rate-limit the actual RPC to once per second; between samples
        # report the cached duration so callers can still threshold.
        if now - self._growth_check_at < 1.0:
            if self._growth_last_grew_at is None: return None
            return now - self._growth_last_grew_at
        self._growth_check_at = now
        try:
            cur_len = len(self.client.rpc("adapter.transcript",
                {"adapter": self.aid, "redact": False}, t=2.0)["text"])
        except (RuntimeError, TimeoutError):
            # RPC failure is a separate signal — don't penalize the
            # stuck timer with it (the caller will surface real RPC
            # death through its own exception handling).
            if self._growth_last_grew_at is None: return None
            return now - self._growth_last_grew_at
        if self._growth_last_len is None:
            # First sample — seed the tracker so subsequent ticks have
            # a baseline to compare against.
            self._growth_last_len = cur_len
            self._growth_last_grew_at = now
            return 0.0
        if cur_len - self._growth_last_len >= PASTE_REACTION_BYTES:
            # Real progress crossed the noise floor — reset the timer
            # and the baseline so the next accumulation starts fresh.
            self._growth_last_len = cur_len
            self._growth_last_grew_at = now
            return 0.0
        # Sub-threshold growth (or none) — DO NOT update
        # `_growth_last_len` so trickle accumulates rather than
        # silently resetting the comparison baseline.
        return now - (self._growth_last_grew_at or now)

    # ─── error metadata accessors ────────────────────────────────────────
    def _last_error_kind(self) -> str | None:
        md = self._last_metadata
        if not md: return None
        err = md.get("error")
        if not isinstance(err, dict): return None
        kind = err.get("kind")
        return kind if isinstance(kind, str) else None

    def _format_error_detail(self) -> str:
        md = self._last_metadata or {}
        err = md.get("error") if isinstance(md.get("error"), dict) else {}
        parts: list[str] = []
        msg = err.get("message")
        if isinstance(msg, str) and msg:
            parts.append(msg)
        retry = err.get("retry_after_s")
        if isinstance(retry, (int, float)) and retry > 0:
            parts.append(f"retry_after={int(retry)}s")
        return " / ".join(parts)

    # ─── compound: stream body deltas until classifier signals completion ─
    def stream_until_done(self) -> int:
        """Stream body deltas + state transitions until a terminal state.

        Termination is classifier-driven:
          * `completed_turn` (success): the TUI rendered its end-of-turn
            marker (`✻ <Verb> for <duration>`) and the input prompt is
            visible without an active-work spinner — i.e. Claude is back
            at idle.
          * `error` / `exited` / `plugin_error` (failure): the host or
            the classifier reported a fatal condition.

        Output strategy:
          * Each heartbeat we read `body_text` and diff against the
            previous body. Lines we have NEVER printed before
            (full-content match — no normalization, no position tricks)
            get streamed to stdout.
          * Spinner / status-bar / pure-chrome lines are filtered by
            structural patterns, not by glyph-stripping, so the dedup
            key is just the line's stripped content.
          * If the turn finishes without surfacing any content (a
            common Haiku failure mode where the model acknowledges and
            stops without using tools), the final body is dumped so the
            user always sees Claude's actual reply.
        """
        body, _ = self.inspect()
        baseline_len = len(body)
        last_body = body
        # Dedup by full stripped line content. Seeded with the
        # pre-stream body so we don't re-emit the welcome chrome or
        # the prompt echo.
        printed_lines: set[str] = set()
        for line in last_body.splitlines():
            s = line.strip()
            if s:
                printed_lines.add(s)
        streamed_anything = False

        while not self._expired():
            try:
                state = self.poll_state()["state"]
                body, ins = self.inspect()
            except (RuntimeError, TimeoutError) as e:
                self._clear_alive()
                emit(RED("✗ stream interrupted"), str(e))
                return 1
            # Activity-scan surface: body for diff/print, full screen
            # (body + status_text) for the ticker. During Explore-subagent
            # runs the main body is frozen but the status bar still
            # repaints with the spinner / token counter, so the ticker
            # needs the union.
            activity_text = self._screen_for_activity(ins)

            for n in self.client.drain_notifs():
                if n.get("method") == "session.exited":
                    self._clear_alive()
                    emit(YELLOW("◌ session.exited"))
                    self._session_exited = True

            if self._session_exited:
                # The PTY child died — no further poll will produce
                # meaningful state. Bail out as `exited` with a failure
                # exit code so callers / CI know this wasn't a clean
                # turn completion.
                self._clear_alive()
                grew_by = max(0, len(body) - baseline_len)
                emit(RED("✗ exited"), DIM(f"(+{grew_by} chars in body; session closed mid-stream)"))
                return 1

            if body != last_body:
                # Walk EVERY line in the new body (not just past common
                # prefix). Position-based diffing missed content when
                # Claude rewrote screen regions in place; full content
                # match is robust to that.
                printed = False
                for line in body.splitlines():
                    s = line.strip()
                    if not s:
                        continue
                    if _is_chrome_line(s):
                        continue
                    # Skip prompt-glyph rows: the submitted prompt echo
                    # was seeded into `printed_lines` at baseline, but
                    # Claude Code 2.1.x also renders a POST-TURN
                    # ghost-text suggestion (`❯ run the tests`) after
                    # completion — that's not answer content and the
                    # fallback paths already filter it. Mirror that here
                    # so the live diff doesn't print it as a content
                    # line before the terminal-state check fires.
                    if s.startswith("❯"):
                        continue
                    if s in printed_lines:
                        continue
                    printed_lines.add(s)
                    if _COMPLETION_MARKER_RE.match(s):
                        self._saw_completion_marker = True
                    if not printed:
                        self._clear_alive()
                        printed = True
                    sys.stdout.write(line + "\n")
                    streamed_anything = True
                if printed:
                    sys.stdout.flush()
                last_body = body

            if state in self.TERMINAL_STATES:
                self._clear_alive()
                grew_by = max(0, len(body) - baseline_len)
                # Fallback: surface the answer region. Two layers, in
                # order of data fidelity:
                #   1. Transcript-based — scans this turn's full PTY
                #      scrollback (not just the visible body). ALWAYS
                #      runs at completion, even if the streaming diff
                #      printed something, because long turns can have
                #      the final answer scroll past between the last
                #      polled body and the terminal-state check —
                #      `streamed_anything = True` alone is not proof
                #      the user saw the actual reply. Already-printed
                #      lines are deduped via the `printed_lines` set
                #      so this only fills gaps, never repeats.
                #   2. Body-based — runs only when the transcript was
                #      empty / unavailable AND the body has grown.
                #      Covers the Haiku-style ack-and-stop case where
                #      transcript baseline wasn't established (test
                #      harness paths).
                transcript_dumped = self._dump_answer_region_from_transcript(
                    already_printed=printed_lines
                )
                if not transcript_dumped and not streamed_anything and grew_by > 0:
                    self._dump_answer_region(body)
                if state == self.SUCCESS_STATE:
                    emit(GREEN(f"✓ {state}"), DIM(f"(+{grew_by} chars in body)"))
                    return self.EXIT_OK
                # error → discriminate on metadata.error.kind so wrappers
                # can swap accounts on a known code. Everything else
                # (exited / plugin_error / generic error) collapses to 1.
                if state == "error":
                    kind = self._last_error_kind()
                    detail = self._format_error_detail()
                    if kind == "rate_limit":
                        emit(RED("✗ rate limit reached"),
                             (DIM(detail) + "  " if detail else "") +
                             DIM("→ exit 3 (swap account or wait and retry)"))
                        return self.EXIT_RATE_LIMIT
                    if kind == "quota":
                        emit(RED("✗ usage / credit quota exhausted"),
                             (DIM(detail) + "  " if detail else "") +
                             DIM("→ exit 4 (swap account or top up credits)"))
                        return self.EXIT_QUOTA
                emit(RED(f"✗ {state}"), DIM(f"(+{grew_by} chars in body)"))
                return self.EXIT_GENERIC

            # Stuck-out: classifier says `thinking` but no PTY bytes
            # have arrived for `stuck_after` seconds. Common causes
            # the classifier alone can't see: silent server-side usage
            # gate (limit hit before banner renders), OAuth refresh
            # mid-request, TLS stall. Bail with EXIT_STUCK so wrappers
            # can distinguish "real hang" from "real error".
            if self.stuck_after > 0 and state == "thinking":
                stuck_for = self._check_idle_growth()
                if stuck_for is not None and stuck_for >= self.stuck_after:
                    self._clear_alive()
                    # Rescue path: if the streaming diff already
                    # surfaced a `✻ <Verb> for <N>s` completion
                    # marker, the turn IS done — the classifier just
                    # got tripped by post-turn tip / spinner
                    # repaints (`⎿ Tip: …`, the next spinner frame
                    # kicking off, etc.). Treat as `completed_turn`
                    # and dump the answer region the same way the
                    # normal terminal-state branch does. Without
                    # this, a successful turn followed by a "did
                    # you know" post-turn callout flakes the script
                    # into a stuck-out failure that wrappers would
                    # wrongly classify as a hang.
                    if self._saw_completion_marker:
                        grew_by = max(0, len(body) - baseline_len)
                        self._dump_answer_region_from_transcript(
                            already_printed=printed_lines
                        )
                        emit(GREEN("✓ completed_turn"),
                             DIM(f"(+{grew_by} chars in body; "
                                 "marker observed via streaming diff, "
                                 "classifier did not flip — likely a "
                                 "post-turn tip / spinner repaint)"))
                        return self.EXIT_OK
                    emit(RED("✗ stuck"),
                         f"no PTY output for {stuck_for:.0f}s while classifier reports "
                         f"`thinking` (--stuck-after-seconds={self.stuck_after:.0f}). "
                         + DIM("possible silent usage-limit gate / network stall; "
                               "try swapping accounts or rerunning"))
                    # Best-effort cancel so the half-started turn doesn't
                    # keep burning server-side resources after we return.
                    try:
                        self.client.rpc("adapter.send",
                            {"adapter": self.aid, "intent": "cancel", "params": {}}, t=2.0)
                    except (RuntimeError, TimeoutError):
                        pass
                    return self.EXIT_STUCK

            self._tick_alive(prefix=f"{state}", body=activity_text)
            time.sleep(self.heartbeat)

        self._clear_alive()
        emit(RED("✗ timed out"), f"after {self.args.timeout:.0f}s without a terminal state")
        return 1

    def close(self):
        if self._closed: return
        self._closed = True
        try:
            self.client.rpc("adapter.close", {"adapter": self.aid}, t=3.0)
        except Exception: pass

# ───────────────────────── main ─────────────────────────────────────────────

def main() -> int:
    args = parse_args()
    prompt = resolve_prompt(args.prompt).rstrip("\n")
    if not prompt.strip():
        print("claude-stream: empty prompt; nothing to send", file=sys.stderr)
        return 2

    bin_path = shutil.which(args.ptywright) or args.ptywright
    if not Path(bin_path).exists():
        print(f"claude-stream: ptywright not found at {bin_path}", file=sys.stderr)
        return 2
    if not shutil.which("claude"):
        print("claude-stream: `claude` binary not found on PATH", file=sys.stderr)
        return 2

    emit(CYAN("▶ claude-stream"),
         DIM(f"cwd={args.cwd} model={args.model or 'default'} heartbeat={args.heartbeat_ms}ms"))
    summary = prompt if len(prompt) <= 200 else prompt[:197] + "..."
    emit(DIM("  prompt:"), summary)
    print()

    client = Client(bin_path, log_level=os.environ.get("PTYWRIGHT_LOG", "warn"))
    stream: Stream | None = None
    aid: str | None = None
    # Created lazily inside the try/finally below so a failure here cannot
    # leak the ptywright subprocess `client` just started.
    direnv_config: tempfile.TemporaryDirectory | None = None

    # SIGINT handler — terminate the server subprocess and let the main
    # thread perform RPC cleanup outside the signal context. Doing RPC
    # from the handler itself risks deadlocking on Client.lock: the
    # handler runs on the main thread, and if SIGINT happens to be
    # delivered while the main thread is inside `with self.lock:` in
    # Client.rpc, re-acquiring the (non-reentrant) lock from the handler
    # would block forever.
    #
    # Terminating the server subprocess breaks any blocking rpc() call
    # in the main thread (the reader thread sets `_dead = True` when
    # stdout closes; the next rpc() raises RuntimeError, which the
    # stream loop already handles by returning 1; main()'s try/finally
    # then runs the actual cleanup with the lock held normally).
    interrupted = {"flag": False}
    def on_sigint(_signum, _frame):
        if interrupted["flag"]:
            os._exit(130)  # second Ctrl+C → hard exit, no cleanup
        interrupted["flag"] = True
        sys.stderr.write(f"\n{YELLOW('⏸ Ctrl+C — terminating ptywright (Ctrl+C again to force)')}\n")
        try: client.proc.terminate()
        except Exception: pass
    signal.signal(signal.SIGINT, on_sigint)

    hard_deadline = time.monotonic() + args.timeout

    try:
        client.rpc("server.set_notifications", {"enabled": True})

        direnv_config = tempfile.TemporaryDirectory(prefix="claude-stream-direnv-")
        direnv_config_path = Path(direnv_config.name)
        (direnv_config_path / "direnv.toml").write_text(
            'log_filter = "^$"\nhide_env_diff = true\n',
            encoding="utf-8",
        )

        start_params: dict = {
            "plugin": "claude-code",
            "cwd": args.cwd,
            "env": {"DIRENV_CONFIG": str(direnv_config_path)},
        }
        if args.model:
            start_params["args"] = ["--model", args.model]
        start = client.rpc("adapter.start", start_params, t=15.0)
        aid = start["adapter"]
        # adapter.start returns the underlying session id too — we use it
        # for session.* calls during streaming so the per-adapter mutex
        # held by the background adapter.wait does not block our snapshots.
        session_id = start.get("session")
        if not session_id:
            emit(RED("✗ adapter.start did not return a session id"))
            return 1
        emit(GREEN("▣ adapter started"),
             DIM(f"id={aid} session={session_id} initial_state={start['state']['state']}"))

        stream = Stream(client, aid, args, hard_deadline)

        # Pre-submit gate. Three buckets of behavior on each tick:
        #
        #   1. Handle: workspace-trust dialog → auto-approve and reloop.
        #   2. Fail fast: terminal failure states (`error`, `plugin_error`)
        #      and the new `waiting_for_login` state surface their
        #      classifier evidence with a non-zero exit instead of
        #      burning the global deadline. `waiting_for_login` is
        #      special-cased: send_prompt can't dismiss it, the user
        #      has to `claude login` first, so we refuse to paste into
        #      a sign-in dialog.
        #   3. Allow-list to proceed: only break out of the loop when
        #      the classifier reports a state where `send_prompt` is
        #      safe to run. Anything else (an unrecognized state, the
        #      classifier's no-evidence fallback) keeps polling — so a
        #      future Claude TUI screen we haven't taught the
        #      classifier about can't race the paste into the wrong
        #      place. The hard deadline still bounds the wait.
        SUBMIT_READY_STATES = {
            "ready",
            "waiting_for_user_input",
            "starting",  # only with the welcome-screen evidence below
            "completed_turn",  # last turn already done; safe to send next
            # Deliberately NOT in this set: `thinking`. Calling
            # `send_prompt` while a prior turn is in flight flips
            # `last_intent` back to `prompt_submitted` and confuses the
            # classifier's mid-turn / completed-turn branches. The
            # plugin exposes `steer` for mid-turn injection (same paste
            # bytes, intent intact); the wrapper here is single-turn
            # and should always wait for `waiting_for_user_input` /
            # `completed_turn` before submitting. The hard deadline
            # still bounds the wait if `--continue`-style state ever
            # gets us stuck in `thinking` indefinitely.
        }
        # Whether the startup loop has seen the classifier report
        # `waiting_for_user_input` (input prompt rendered). If yes,
        # `submit()` can skip its initial settle wait — see the
        # docstring there.
        input_prompt_seen = False
        while not stream._expired():
            st = stream.poll_state()
            state = st["state"]
            evidence = st.get("evidence") or ""
            if state == "waiting_for_trust":
                emit(YELLOW("? workspace-trust dialog detected, auto-approving"))
                client.rpc("adapter.send",
                           {"adapter": aid, "intent": "approve_trust", "params": {}}, t=5.0)
                continue
            if state in {"error", "plugin_error"}:
                emit(RED(f"✗ adapter entered {state} during startup"), DIM(evidence))
                stream.close()
                return 1
            if state == "waiting_for_login":
                emit(RED("✗ Claude requires sign-in"),
                     DIM("run `claude` interactively once, complete the login flow, then retry"))
                stream.close()
                return 2
            # The classifier's no-evidence fallbacks aren't safe to
            # submit into — shell / direnv chatter before Claude renders
            # its prompt lands here. Keep polling.
            if "no screen evidence" in evidence or "no Claude Code-specific evidence" in evidence:
                time.sleep(stream.heartbeat)
                continue
            # Welcome panel is a green light — send_prompt's leading
            # Enter dismisses it.
            if state == "starting" and "welcome screen visible" in evidence:
                break
            if state == "waiting_for_user_input":
                input_prompt_seen = True
                break
            if state in SUBMIT_READY_STATES:
                break
            # Unknown state — keep polling rather than guessing. The
            # hard deadline bounds this.
            time.sleep(stream.heartbeat)

        ok, reason = stream.submit(prompt, input_prompt_already_observed=input_prompt_seen)
        if not ok:
            emit(RED("✗ could not get Claude to accept the prompt"), DIM(reason))
            stream.close()
            return 1

        rc = stream.stream_until_done()
        stream.close()
        return rc

    finally:
        client.close()
        if direnv_config is not None:
            direnv_config.cleanup()

if __name__ == "__main__":
    sys.exit(main())
