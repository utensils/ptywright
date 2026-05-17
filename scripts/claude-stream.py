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
#   * `_CHROME_SEARCHED_RE` — Claude Code progress lines like
#     `Searching for 1 pattern…` / `Searched for 1 pattern (ctrl+o
#     to expand)`. These mutate (count, "ing" → "ed") every tick and
#     would otherwise be deduped per state.
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
    r"^(?:Searching|Searched|Reading|Read|Listing|Listed|Glob|Grep)\b.*\(ctrl\+o to expand\)$"
)

def _is_chrome_line(s: str) -> bool:
    if _CHROME_RULE_RE.match(s):
        return True
    if _CHROME_SPINNER_RE.match(s) and len(s) <= 120:
        return True
    if _CHROME_SEARCHED_RE.match(s):
        return True
    return False

class Stream:
    """Robust Claude Code streaming driver — TUI-version-agnostic."""

    SUCCESS_STATE = "completed_turn"
    # Any of these end the stream. Only SUCCESS_STATE is a clean exit;
    # `error` / `exited` / `plugin_error` are failure terminations and
    # must return a non-zero exit code so CI / callers don't treat
    # them as success.
    TERMINAL_STATES = {"completed_turn", "error", "exited", "plugin_error"}

    def __init__(self, client: Client, aid: str, args: argparse.Namespace,
                 hard_deadline: float):
        self.client = client
        self.aid = aid
        self.args = args
        self.heartbeat = args.heartbeat_ms / 1000.0
        self.deadline = hard_deadline
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
        # Alive ticker — overwrites in place on a TTY.
        self._last_alive_at = 0.0
        self._alive_phase = 0

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

    # ─── primitive: poll state once, annotate transitions ────────────────
    def poll_state(self) -> dict:
        st = self.client.rpc("adapter.state", {"adapter": self.aid}, t=5.0)
        s = st["state"]["state"]
        ev = st["state"].get("evidence", "")
        md = st["state"].get("metadata")
        if not self.args.quiet:
            # Only log on actual state transitions. Evidence can flap
            # within a single state (e.g. the mid-turn `thinking` branch
            # alternates between "active work indicator detected" and
            # "turn in flight; no completion marker on screen" as the
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
    def _tick_alive(self, prefix: str = ""):
        if not sys.stdout.isatty(): return
        now = time.monotonic()
        # Throttle to at most one tick per 4 heartbeats so the spinner
        # cadence is independent of how aggressively we poll.
        if now - self._last_alive_at < self.heartbeat * 4: return
        self._last_alive_at = now
        self._alive_phase = (self._alive_phase + 1) % 8
        glyph = "⠋⠙⠹⠸⠼⠴⠦⠧"[self._alive_phase]
        elapsed = self.args.timeout - self._budget()
        sys.stdout.write(f"\r{DIM(f'{glyph} {prefix} [{elapsed:.0f}s]')}\033[K")
        sys.stdout.flush()

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

        The "answer region" is everything from after the last `❯ …`
        prompt-with-text line (the user's submitted prompt echo) up to
        (but not including) the trailing empty `❯` idle prompt. That
        captures Claude's actual reply plus the completion marker
        without the welcome chrome above or the post-turn input row
        below. Chrome lines (horizontal rules, spinner status) are
        skipped using the same structural filter the streaming loop
        uses.
        """
        lines = body.splitlines()
        # Find the user's submitted prompt echo: a line starting with
        # the prompt glyph followed by user text. The answer starts
        # AFTER that.
        start = 0
        for i, line in enumerate(lines):
            s = line.strip()
            if (s.startswith("❯") or s.startswith(">")) and len(s) > 2 and not s[1:].lstrip().startswith(("1.", "2.", "3.")):
                start = i + 1
        # Find the trailing idle prompt (a line that's JUST `❯` or `>`).
        # Everything between `start` and that boundary is the answer.
        end = len(lines)
        for i in range(len(lines) - 1, start, -1):
            s = lines[i].strip()
            if s in ("❯", ">"):
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
    def submit(self, prompt: str) -> tuple[bool, str]:
        """Submit the prompt. The plugin's `send_prompt` intent now emits
        Enter → BracketedPaste → Enter, which both dismisses any
        first-keypress interceptor (welcome panel, compact-launch view)
        AND submits the prompt. So the script's responsibility shrinks
        to: wait until the screen is stable, then send_prompt.

        Verification: poll the transcript for growth above noise. A real
        paste acceptance produces 1000+ bytes of PTY output within the
        first heartbeat; silent absorption produces only idle status-bar
        noise (~120 bytes per tick). PASTE_REACTION_BYTES sits between.

        Returns (success, failure_reason).
        """
        emit(DIM("· waiting for Claude's input box to settle"))
        self.wait_for_settled(max_wait_s=3.0)
        if self._expired(): return False, "deadline expired before initial settle"

        baseline = len(self.client.rpc("adapter.transcript",
            {"adapter": self.aid, "redact": False}, t=5.0)["text"])

        self.send_prompt(prompt)

        while not self._expired():
            trans = self.client.rpc("adapter.transcript",
                {"adapter": self.aid, "redact": False}, t=5.0)["text"]
            grew = len(trans) - baseline
            if grew >= PASTE_REACTION_BYTES:
                emit(GREEN("→ prompt submitted"),
                     DIM(f"({len(prompt)} chars; +{grew} bytes from Claude)"))
                return True, ""
            self._tick_alive(f"waiting for Claude to acknowledge paste (+{grew}B)")
            time.sleep(self.heartbeat)
        return False, "deadline expired waiting for Claude to acknowledge paste"

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
                body, _ = self.inspect()
            except (RuntimeError, TimeoutError) as e:
                self._clear_alive()
                emit(RED("✗ stream interrupted"), str(e))
                return 1

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
                    if s in printed_lines:
                        continue
                    printed_lines.add(s)
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
                # Fallback: surface the answer region even if streaming
                # caught nothing. Happens when Claude only added the
                # completion marker (Haiku ack-and-stop) or when content
                # arrived between the last diff and the terminal-state
                # check. The user always sees what's on screen.
                if not streamed_anything and grew_by > 0:
                    self._dump_answer_region(body)
                if state == self.SUCCESS_STATE:
                    emit(GREEN(f"✓ {state}"), DIM(f"(+{grew_by} chars in body)"))
                    return 0
                # error / exited / plugin_error → failure exit.
                emit(RED(f"✗ {state}"), DIM(f"(+{grew_by} chars in body)"))
                return 1

            self._tick_alive(prefix=f"{state}")
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
            "thinking",  # rare; Claude finishing a prior turn from --continue
            "completed_turn",  # last turn already done; safe to send next
        }
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
            if state in SUBMIT_READY_STATES:
                break
            # Unknown state — keep polling rather than guessing. The
            # hard deadline bounds this.
            time.sleep(stream.heartbeat)

        ok, reason = stream.submit(prompt)
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
