#!/usr/bin/env python3
"""claude-stream — drive Claude Code through ptywright and stream the result.

Usage:
  claude-stream "your prompt"
  claude-stream @path/to/prompt.md
  claude-stream -                # read prompt from stdin
  claude-stream --help

Spawns `ptywright serve --stdio`, starts the built-in `claude-code` adapter,
auto-handles the workspace-trust dialog if it appears, submits the prompt,
and streams the live screen body + state transitions until the turn
completes. Demonstrates the session.output streaming pattern documented in
.claude/skills/ptywright/SKILL.md.

Requires `claude` on PATH and `ptywright` either on PATH or via
$PTYWRIGHT_BIN / the devshell wrapper.
"""
from __future__ import annotations

import argparse
import json
import os
import queue
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

# ----- terminal styling -------------------------------------------------------

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
    sys.stdout.write(f"{prefix} {body}\n" if body else f"{prefix}\n")
    sys.stdout.flush()

# ----- arg parsing ------------------------------------------------------------

def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        prog="claude-stream",
        description="Drive Claude Code via ptywright and stream the response.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    ap.add_argument("prompt", help='prompt string, @path/to/file, or - for stdin')
    ap.add_argument("--cwd", default=os.getcwd(), help="working directory for the claude session (default: $PWD)")
    ap.add_argument("--model", default="haiku",
                    help="model passed to `claude --model` (default: haiku — fastest for smoke tests; pass --model '' to let claude choose)")
    ap.add_argument("--timeout", type=float, default=600.0, help="overall timeout in seconds (default: 600)")
    ap.add_argument("--heartbeat-ms", type=int, default=80,
                    help="poll cadence for session.output notifications (lower = lower latency, higher CPU; default: 80)")
    ap.add_argument("--ptywright", default=os.environ.get("PTYWRIGHT_BIN", "ptywright"),
                    help="path to the ptywright binary (default: $PTYWRIGHT_BIN or `ptywright`)")
    ap.add_argument("--no-trust", action="store_true", help="do not auto-approve the workspace-trust dialog")
    return ap.parse_args()

def resolve_prompt(arg: str) -> str:
    if arg == "-":
        return sys.stdin.read()
    if arg.startswith("@"):
        return Path(arg[1:]).read_text(encoding="utf-8")
    return arg

# ----- jsonrpc client ---------------------------------------------------------

class Client:
    """Minimal NDJSON JSON-RPC client over a subprocess pipe.

    Background reader thread parses each line and dispatches:
      - id-bearing responses to a per-call queue (rpc(...) is synchronous)
      - notifications to an internal list (drained by the main loop)
    """
    def __init__(self, bin_path: str):
        self.proc = subprocess.Popen(
            [bin_path, "serve", "--stdio"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
            env={**os.environ, "PTYWRIGHT_LOG": os.environ.get("PTYWRIGHT_LOG", "warn")},
        )
        self.responses: dict[int, queue.Queue] = {}
        self.notifs: list[dict] = []
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
                except json.JSONDecodeError:
                    sys.stderr.write(f"[ptywright non-JSON] {line!r}\n"); continue
                if "id" in m:
                    with self.lock:
                        q = self.responses.get(m["id"])
                    if q: q.put(m)
                else:
                    with self.lock:
                        self.notifs.append(m)
        finally:
            self._dead = True

    def _stderr_reader(self):
        for line in self.proc.stderr:
            sys.stderr.write(f"{DIM('[ptywright]')} {line}")

    def rpc(self, method: str, params: dict | None = None, t: float = 30.0):
        if self._dead:
            raise RuntimeError("ptywright server has exited")
        self._rid += 1
        rid = self._rid
        q: queue.Queue = queue.Queue()
        with self.lock: self.responses[rid] = q
        req: dict = {"jsonrpc":"2.0","id":rid,"method":method}
        if params: req["params"] = params
        self.proc.stdin.write(json.dumps(req) + "\n"); self.proc.stdin.flush()
        try:
            msg = q.get(timeout=t)
        finally:
            with self.lock: self.responses.pop(rid, None)
        if "error" in msg:
            raise RuntimeError(f"{method} failed: {msg['error']}")
        return msg["result"]

    def drain_notifs(self) -> list[dict]:
        with self.lock:
            out, self.notifs = self.notifs, []
        return out

    def close(self):
        try: self.proc.stdin.close()
        except: pass
        try: self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()

# ----- streaming loop ---------------------------------------------------------

TERMINAL_STATES = {"completed_turn", "error", "exited", "plugin_error"}

def main() -> int:
    args = parse_args()
    prompt = resolve_prompt(args.prompt).rstrip("\n")
    if not prompt.strip():
        print("claude-stream: empty prompt; nothing to send", file=sys.stderr)
        return 2

    bin_path = shutil.which(args.ptywright) or args.ptywright
    if not Path(bin_path).exists():
        print(f"claude-stream: ptywright not found at {bin_path}", file=sys.stderr)
        print("  hint: run `build-release` in the devshell, then export "
              "PTYWRIGHT_BIN=$PRJ_ROOT/target/release/ptywright", file=sys.stderr)
        return 2
    if not shutil.which("claude"):
        print("claude-stream: `claude` binary not found on PATH", file=sys.stderr)
        return 2

    emit(CYAN("▶ claude-stream"), DIM(f"cwd={args.cwd} heartbeat={args.heartbeat_ms}ms"))
    emit(DIM("  prompt:"), prompt if len(prompt) <= 200 else prompt[:197] + "...")
    print()

    client = Client(bin_path)
    try:
        client.rpc("server.set_notifications", {"enabled": True})

        # adapter.start — the built-in plugin's default_target supplies
        # program="claude" + rows=60 cols=200 (the classifier-stable preset).
        start_params: dict = {"plugin": "claude-code", "cwd": args.cwd}
        # Default to haiku for fast, deterministic smoke tests; allow the
        # caller to opt out with --model ''.
        if args.model:
            start_params["args"] = ["--model", args.model]
        start = client.rpc("adapter.start", start_params, t=15.0)
        aid = start["adapter"]
        emit(GREEN(f"▣ adapter started"), DIM(f"id={aid} state={start['state']['state']}"))

        # Pump notifications + state transitions
        deadline = time.monotonic() + args.timeout
        last_body = ""
        last_body_change_at = time.monotonic()
        last_state = start["state"]["state"]
        last_evidence = start["state"].get("evidence", "")
        prompt_sent = False
        prompt_sent_at = 0.0
        last_metadata: dict | None = None
        # Verb-agnostic indicators. Claude Code rotates the spinner verb
        # per turn (`Generating…`, `Cogitating…`, `Zigzagging…`) so we
        # look at the glyphs that wrap them. The trailing `…` only
        # appears in active-work lines, and `⏺ ` is the answer-block
        # bullet glyph Claude renders for every assistant message.
        SPINNER_GLYPHS = "✶✻✽✢✳·✷✸✹✺✼✠✦✯◆"
        def has_spinner(text: str) -> bool:
            return any(g in text for g in SPINNER_GLYPHS) and "…" in text
        def has_answer_bullet(text: str) -> bool:
            return "⏺ " in text

        while time.monotonic() < deadline:
            # Heartbeat to flush notifications + read fresh state
            try:
                st = client.rpc("adapter.state", {"adapter": aid}, t=5.0)
            except RuntimeError as e:
                emit(RED("✗ adapter.state failed"), str(e))
                return 1
            state = st["state"]["state"]
            evidence = st["state"].get("evidence", "")
            confidence = st["state"].get("confidence", 0.0)
            md = st["state"].get("metadata")

            # Drain session.output notifications. We use them as a liveness
            # signal — the human-readable view comes from adapter.inspect
            # below, since the raw PTY bytes include ANSI escapes and cursor
            # positioning that would corrupt our annotated output stream.
            for n in client.drain_notifs():
                if n.get("method") == "session.exited":
                    emit(YELLOW("◌ session.exited"))

            # When the state or evidence changes, announce it
            if state != last_state or evidence != last_evidence:
                emit(MAGENTA(f"⇢ state"), f"{last_state} → {BOLD(state)}  {DIM(evidence)}")
                last_state = state
                last_evidence = evidence

            # Auto-handle workspace-trust
            if state == "waiting_for_trust" and not args.no_trust:
                emit(YELLOW("? workspace-trust dialog detected, auto-approving"))
                client.rpc("adapter.send", {"adapter": aid, "intent": "approve_trust", "params": {}}, t=5.0)
                time.sleep(0.5)
                continue

            # Submit the prompt once we've reached a state Claude will
            # actually accept typed input from. Three valid launching pads:
            #   • `waiting_for_user_input`: real prompt glyph (❯) on screen
            #   • `ready` with high confidence: matched a welcome anchor
            #   • `starting` with "welcome screen visible" evidence: the
            #     post-trust welcome panel. The skill documents that
            #     send_prompt's bracketed-paste both dismisses the panel
            #     AND submits in one step, so we don't need a separate
            #     dismiss_welcome intent (which on Claude Code 2.1.143
            #     does not reliably clear the panel on its own).
            ready_for_prompt = (
                state == "waiting_for_user_input"
                or (state == "ready" and confidence >= 0.5)
                or (state == "starting" and "welcome" in evidence)
            )
            if not prompt_sent and ready_for_prompt:
                client.rpc("adapter.send", {
                    "adapter": aid, "intent": "send_prompt", "params": {"prompt": prompt}
                }, t=10.0)
                emit(GREEN("→ send_prompt"), DIM(f"({len(prompt)} chars)"))
                prompt_sent = True
                prompt_sent_at = time.monotonic()
                # Give Claude a beat to acknowledge the paste before we
                # start polling body_text — otherwise the first inspect
                # may still show the welcome panel and confuse the diff.
                time.sleep(0.3)
                continue

            # Stream the body text — print only what's new this tick.
            # We deliberately poll body_text post-send even when the
            # classifier still reports `starting (welcome)` — on Claude
            # Code 2.1.143 the welcome panel can persist visually even
            # after a turn is underway, so trusting the state alone would
            # silence the live stream.
            if prompt_sent:
                inspect = client.rpc("adapter.inspect", {"adapter": aid, "redact": False}, t=5.0)
                body = inspect.get("body_text", "")
                if body != last_body:
                    old_lines = last_body.splitlines()
                    new_lines = body.splitlines()
                    common = 0
                    for o, n in zip(old_lines, new_lines):
                        if o == n: common += 1
                        else: break
                    for line in new_lines[common:]:
                        s = line.strip()
                        if not s: continue
                        if s.startswith("─") and s.rstrip("─") == "": continue
                        if has_spinner(s) and len(s) < 40: continue
                        sys.stdout.write(line + "\n")
                    sys.stdout.flush()
                    # For stability detection, ignore the spinner line
                    # (its glyph + ticking duration changes every poll
                    # even when no real progress is happening). Compare
                    # against the previous spinner-stripped body so
                    # `last_body_change_at` only advances on actual
                    # progress (new tokens, new tool call, new prompt).
                    def strip_spinner(t: str) -> str:
                        return "\n".join(
                            ln for ln in t.splitlines()
                            if not (has_spinner(ln) and len(ln.strip()) < 80)
                        )
                    if strip_spinner(body) != strip_spinner(last_body):
                        last_body_change_at = time.monotonic()
                    last_body = body

            # Surface metadata when it appears
            if md and md != last_metadata:
                if "permission" in md:
                    p = md["permission"]
                    emit(YELLOW("⚠ permission requested"),
                         f"tool={p.get('tool','?')} summary={p.get('summary','?')[:80]}")
                    emit(DIM("   options"), ", ".join(p.get("options", [])))
                if "status" in md and (not last_metadata or last_metadata.get("status") != md["status"]):
                    s = md["status"]
                    emit(DIM("  status"),
                         f"model={s.get('model','?')} mode={s.get('permission_mode','?')}")
                if "usage" in md:
                    u = md["usage"]
                    emit(GREEN("$ usage"),
                         " ".join(f"{k}={v}" for k, v in u.items() if v is not None))
                last_metadata = md

            # Turn-completion detection. With the classifier fix that
            # ignores stale welcome chrome once a prompt has been
            # submitted, `state == "completed_turn"` is the canonical
            # signal — it fires from the state-poll path (lower
            # confidence) as soon as the spinner clears, and again from
            # the stable path (higher confidence) once the screen has
            # settled. We also accept terminal error/exit states.
            if prompt_sent and state in TERMINAL_STATES:
                emit(GREEN(f"✓ {state}"), DIM(evidence))
                client.rpc("adapter.close", {"adapter": aid}, t=5.0)
                return 0

            time.sleep(args.heartbeat_ms / 1000.0)

        emit(RED("✗ timed out"), f"after {args.timeout}s; last state {last_state}")
        client.rpc("adapter.close", {"adapter": aid}, t=5.0)
        return 1
    except KeyboardInterrupt:
        emit(YELLOW("⏸ interrupted"))
        try: client.rpc("adapter.close", {"adapter": aid}, t=3.0)
        except Exception: pass
        return 130
    finally:
        client.close()

if __name__ == "__main__":
    sys.exit(main())
