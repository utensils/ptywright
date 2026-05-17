"""Tests for scripts/claude-stream.py.

Run with `python3 -m unittest tests/scripts/test_claude_stream.py` from
the repo root. Stdlib-only (no pytest dependency) so the test suite stays
inside the devshell's `python3` without needing pip / Nix overlay churn.

The script is loaded via importlib because its filename has a hyphen
(`claude-stream.py`), which can't be imported with `import`.

Coverage:
  * `_is_chrome_line` and the underlying regex shapes — including the
    spinner-glyph-set guard that keeps prose like "Done… (for now)"
    from being filtered.
  * `Stream._dump_answer_region` — fallback content surfacing when the
    streaming diff caught nothing.
  * `Stream.TERMINAL_STATES` semantics — every member is a `completed_turn`
    success or a defined failure code path.
  * `Client` JSON-RPC framing — request/response correlation, error
    propagation, notification fan-out — driven by an in-test fake
    server speaking NDJSON over the script's spawn channel.

End-to-end coverage of the startup loop and `stream_until_done` lives
in `tests/cli_tests.rs` (the Rust side covers the host) and is
exercised manually with `nix develop --command claude-stream`. This
suite locks the script-side contracts that determine which body
content the user actually sees and how failures terminate.
"""

from __future__ import annotations

import importlib.util
import io
import json
import os
import queue
import sys
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
CLAUDE_STREAM_PATH = REPO_ROOT / "scripts" / "claude-stream.py"


def _load_claude_stream() -> ModuleType:
    """Load `scripts/claude-stream.py` as an importable module."""
    spec = importlib.util.spec_from_file_location(
        "claude_stream", CLAUDE_STREAM_PATH
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    # Ensure top-level `if __name__ == "__main__"` block doesn't fire.
    spec.loader.exec_module(module)
    return module


CS = _load_claude_stream()


class IsChromeLineTests(unittest.TestCase):
    """Structural filter for body chrome — must surface real content
    and hide TUI decoration without false-matching legitimate prose."""

    def test_horizontal_rule_is_chrome(self):
        self.assertTrue(CS._is_chrome_line("─" * 60))
        self.assertTrue(CS._is_chrome_line("━" * 40))
        self.assertTrue(CS._is_chrome_line("═══"))

    def test_horizontal_rule_with_text_is_not_chrome(self):
        # A line with a horizontal-rule char mixed with text is content.
        self.assertFalse(CS._is_chrome_line("─── Section ───"))

    def test_spinner_line_is_chrome(self):
        # Glyph from the spinner set + verb ending in ellipsis.
        self.assertTrue(CS._is_chrome_line("✶ Razzle-dazzling…"))
        self.assertTrue(CS._is_chrome_line("✻ Brewing…"))
        self.assertTrue(CS._is_chrome_line("⠋ Working…"))

    def test_spinner_line_with_counter_is_chrome(self):
        # The parenthesized counter that mutates every frame.
        self.assertTrue(CS._is_chrome_line("✶ Bunning… (9s · ↑ 217 tokens · thinking)"))
        self.assertTrue(CS._is_chrome_line("⏺ Reading 1 file… (ctrl+o to expand)"))

    def test_legitimate_prose_with_ellipsis_is_not_chrome(self):
        # Regression for the bug peer review flagged: the previous
        # regex `^\S+\s+\S+…(...)?$` would match prose like "Done…
        # (for now)". Now that the leading glyph must be in the
        # spinner set, ordinary words don't match.
        self.assertFalse(CS._is_chrome_line("Done… (for now)"))
        self.assertFalse(CS._is_chrome_line("One moment… (please)"))
        self.assertFalse(CS._is_chrome_line("Patience… (this is slow)"))

    def test_completion_marker_is_not_chrome(self):
        # The `✻ <Verb> for <N>` end-of-turn marker MUST surface — it's
        # the user's signal that the turn finished. It doesn't end with
        # an ellipsis so it shouldn't match the spinner shape.
        self.assertFalse(CS._is_chrome_line("✻ Brewed for 1s"))
        self.assertFalse(CS._is_chrome_line("✻ Worked for 5s"))
        self.assertFalse(CS._is_chrome_line("✻ Sautéed for 11s"))

    def test_in_flight_searched_progress_line_is_chrome(self):
        # IN-FLIGHT progress lines (ellipsis + ctrl+o suffix) mutate
        # every tick and would flood the output if not filtered.
        self.assertTrue(CS._is_chrome_line("Searching for 1 pattern… (ctrl+o to expand)"))
        self.assertTrue(CS._is_chrome_line("Reading 3 files… (ctrl+o to expand)"))
        self.assertTrue(CS._is_chrome_line("Listing entries… (ctrl+o to expand)"))

    def test_completed_tool_call_summary_is_not_chrome(self):
        # COMPLETED tool-call summaries (no ellipsis, past tense or
        # stable count) are the informative lines a user wants to see
        # during a long thinking phase. Regression for the
        # `(ctrl+o to expand)` blanket filter that was suppressing all
        # tool-call activity.
        self.assertFalse(CS._is_chrome_line("Searched for 1 pattern (ctrl+o to expand)"))
        self.assertFalse(CS._is_chrome_line("Read 5,103 lines (ctrl+o to expand)"))
        self.assertFalse(CS._is_chrome_line("Found 23 files (ctrl+o to expand)"))

    def test_answer_bullet_line_is_not_chrome(self):
        # `⏺ <answer text>` is real content — must surface.
        self.assertFalse(CS._is_chrome_line("⏺ Here is the answer."))
        # The bullet without the spinner shape is content too.
        self.assertFalse(CS._is_chrome_line("⏺ 28"))

    def test_user_prompt_echo_is_not_chrome(self):
        # The submitted-prompt echo IS structural but isn't filtered
        # by `_is_chrome_line` — the streaming loop dedupes it via the
        # printed_lines baseline seed instead.
        self.assertFalse(CS._is_chrome_line("❯ List the .rs files."))


class ExtractRecentActivityTests(unittest.TestCase):
    """The alive ticker shows the most recent tool-call activity so a
    long `thinking` phase looks alive instead of hung. Without this,
    the user couldn't tell whether Claude was working or stuck."""

    def test_returns_in_flight_searched_line(self):
        body = "\n".join([
            "❯ Explore the project.",
            "",
            "⏺ I'll explore the project structure.",
            "Reading 2 files… (ctrl+o to expand)",
            "",
            "✻ Brewing… (12s · ↑ 217 tokens · esc to interrupt)",
        ])
        # Bottom-up scan returns the spinner first.
        activity = CS._extract_recent_activity(body)
        self.assertIsNotNone(activity)
        self.assertIn("Brewing", activity)

    def test_returns_tool_call_bullet_when_no_spinner(self):
        body = "\n".join([
            "❯ Read the lib file.",
            "",
            "⏺ Read(plugins/claude-code/main.lua)",
            "",
        ])
        activity = CS._extract_recent_activity(body)
        self.assertEqual(activity, "⏺ Read(plugins/claude-code/main.lua)")

    def test_returns_none_when_nothing_recognisable(self):
        body = "\n".join([
            "❯ Hello.",
            "",
            "Plain prose without any tool activity.",
        ])
        self.assertIsNone(CS._extract_recent_activity(body))

    def test_skips_horizontal_rules(self):
        body = "\n".join([
            "⏺ Bash(cargo test)",
            "─" * 60,
            "",
        ])
        activity = CS._extract_recent_activity(body)
        self.assertEqual(activity, "⏺ Bash(cargo test)")


class DumpAnswerRegionTests(unittest.TestCase):
    """Fallback that surfaces the answer when the streaming loop's
    body-diff path caught nothing (very brief reply, in-place rewrite)."""

    def _make_stream(self, captured: io.StringIO) -> "CS.Stream":
        # Build a Stream object without invoking the real Client.
        stream = CS.Stream.__new__(CS.Stream)
        stream._closed = False
        stream._last_alive_at = 0.0
        # _clear_alive only writes if stdout is a TTY; here it's a
        # StringIO so the branch is a no-op.
        return stream

    def test_extracts_lines_between_user_prompt_and_idle_prompt(self):
        captured = io.StringIO()
        stream = self._make_stream(captured)
        body = "\n".join([
            "Welcome chrome",
            "",
            "❯ List the rust files.",  # user's submitted prompt echo
            "",
            "⏺ Here are the files:",
            "  src/lib.rs",
            "  src/main.rs",
            "",
            "✻ Brewed for 1s",
            "",
            "❯",  # trailing idle prompt
            "status bar",
        ])
        with mock.patch.object(sys, "stdout", captured):
            stream._dump_answer_region(body)
        out = captured.getvalue()
        self.assertIn("⏺ Here are the files:", out)
        self.assertIn("src/lib.rs", out)
        self.assertIn("src/main.rs", out)
        self.assertIn("✻ Brewed for 1s", out)
        # Welcome chrome above the user prompt must NOT leak in.
        self.assertNotIn("Welcome chrome", out)
        # The status bar below the trailing idle prompt must NOT leak.
        self.assertNotIn("status bar", out)

    def test_handles_body_with_no_idle_prompt(self):
        # When the trailing idle prompt isn't present (rare, partial
        # render), dump everything after the submitted prompt.
        captured = io.StringIO()
        stream = self._make_stream(captured)
        body = "\n".join([
            "❯ Tell me.",
            "",
            "⏺ Brief answer.",
        ])
        with mock.patch.object(sys, "stdout", captured):
            stream._dump_answer_region(body)
        out = captured.getvalue()
        self.assertIn("⏺ Brief answer.", out)

    def test_post_turn_ghost_suggestion_does_not_swallow_answer(self):
        # Claude Code 2.1.x renders a post-turn suggested follow-up
        # prompt (`❯ run the tests`) BELOW the answer/marker. The
        # previous _dump_answer_region used the LAST prompt-with-text
        # line as the start boundary, so it skipped past the answer
        # and printed nothing. Anchoring on the FIRST prompt-with-text
        # (the user's submitted prompt echo at the top) fixes it.
        captured = io.StringIO()
        stream = self._make_stream(captured)
        body = "\n".join([
            "❯ List exactly three .rs files.",  # submitted prompt
            "",
            "⏺ Here are three .rs files:",
            "  src/lib.rs",
            "  src/main.rs",
            "  src/action.rs",
            "",
            "✻ Brewed for 2s",
            "",
            "❯ run the tests",  # post-turn ghost suggestion (NBSP-padded)
        ])
        with mock.patch.object(sys, "stdout", captured):
            stream._dump_answer_region(body)
        out = captured.getvalue()
        self.assertIn("⏺ Here are three .rs files:", out)
        self.assertIn("src/lib.rs", out)
        self.assertIn("src/main.rs", out)
        self.assertIn("src/action.rs", out)
        self.assertIn("✻ Brewed for 2s", out)
        # The ghost suggestion must NOT leak as content.
        self.assertNotIn("run the tests", out)

    def test_skips_chrome_lines_in_answer_region(self):
        captured = io.StringIO()
        stream = self._make_stream(captured)
        body = "\n".join([
            "❯ Do something.",
            "",
            "⏺ Working…",  # spinner-glyph + ellipsis → chrome
            "─" * 60,  # horizontal rule → chrome
            "⏺ Real answer.",
            "✻ Brewed for 2s",
            "❯",
        ])
        with mock.patch.object(sys, "stdout", captured):
            stream._dump_answer_region(body)
        out = captured.getvalue()
        self.assertIn("⏺ Real answer.", out)
        self.assertIn("✻ Brewed for 2s", out)
        # Horizontal rule must not leak.
        self.assertNotIn("─" * 60, out)


class TerminalStatesTests(unittest.TestCase):
    """The TERMINAL_STATES set is the script's source of truth for
    "stop streaming and return". Every member needs a defined
    exit-code mapping."""

    def test_completed_turn_is_success(self):
        self.assertEqual(CS.Stream.SUCCESS_STATE, "completed_turn")
        self.assertIn("completed_turn", CS.Stream.TERMINAL_STATES)

    def test_failure_states_present(self):
        # These three are the host- or classifier-level failure
        # terminations. `exited` is set by the session.exited
        # notification handler, not by the classifier directly.
        for state in ("error", "exited", "plugin_error"):
            self.assertIn(state, CS.Stream.TERMINAL_STATES,
                          f"{state} must be in TERMINAL_STATES for fail-fast exit")


class ChromeSpinnerGlyphSetTests(unittest.TestCase):
    """The Lua plugin's `SPINNER_GLYPHS` table and the script's
    `_CHROME_SPINNER_GLYPHS` char class must overlap. They don't have
    to be identical — Lua tracks all glyphs the classifier might see
    while the script needs broader coverage to filter the streaming
    output — but every Lua glyph must be in the Python set, otherwise
    the streaming loop will leak the spinner frames the classifier
    relies on to detect active work."""

    LUA_SPINNERS = ["✶", "✻", "✺", "✦", "·", "•",
                    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]

    def test_python_chrome_set_covers_lua_spinner_set(self):
        for glyph in self.LUA_SPINNERS:
            self.assertIn(
                glyph, CS._CHROME_SPINNER_GLYPHS,
                f"Lua spinner glyph `{glyph}` is not in _CHROME_SPINNER_GLYPHS; "
                "the streaming loop will leak spinner frames the classifier "
                "filters as active work"
            )


class ClientJsonRpcFramingTests(unittest.TestCase):
    """The Client class spawns ptywright as a subprocess and talks
    NDJSON JSON-RPC. We test its framing/correlation in isolation by
    swapping in a fake `subprocess.Popen` that exposes OS-pipe-backed
    byte streams.

    The reader thread (Client._reader) drops id-bearing messages
    whose ids aren't yet registered in `client.responses`. Pre-staging
    a response onto the stdout pipe BEFORE rpc() runs therefore races
    against the reader — by the time rpc() registers its queue, the
    message may already be gone. To pin the framing contract
    deterministically, each test:
      1. Runs rpc() in a background thread (so it doesn't block the
         test).
      2. Reads the request line from the in_r side of the pipe
         (blocking; guaranteed to see the request the client just
         wrote because writes are line-buffered).
      3. Writes the matching response on out_w.
      4. Joins the rpc() thread.
    The request-read step is the synchronization barrier — by the
    time the response is written, the client has already registered
    its response queue under the right id.

    Contracts pinned here: ID-keyed response correlation, error
    propagation as `RuntimeError` (with the error text retained),
    notification fan-out to `drain_notifs`.
    """

    def _make_pipes_and_client(self):
        import threading

        in_r, in_w = os.pipe()
        out_r, out_w = os.pipe()
        fake_proc = mock.Mock()
        fake_proc.stdin = os.fdopen(in_w, "w", encoding="utf-8", buffering=1)
        fake_proc.stdout = os.fdopen(out_r, "r", encoding="utf-8")
        fake_proc.stderr = io.StringIO()
        fake_proc.poll.return_value = None

        client = CS.Client.__new__(CS.Client)
        client.proc = fake_proc
        client.lock = threading.Lock()
        client._rid = 0
        client.responses = {}
        client.notifs = queue.Queue()
        client._dead = False
        threading.Thread(target=client._reader, daemon=True).start()

        in_r_file = os.fdopen(in_r, "r", encoding="utf-8")

        def write_response(payload):
            os.write(out_w, (json.dumps(payload) + "\n").encode("utf-8"))

        def cleanup():
            client._dead = True
            try: in_r_file.close()
            except Exception: pass
            try: os.close(out_w)
            except Exception: pass

        return client, in_r_file, write_response, cleanup

    def test_response_routes_back_to_caller_by_id(self):
        import threading

        client, in_r_file, write_response, cleanup = self._make_pipes_and_client()
        try:
            slot = {}
            def driver():
                try:
                    slot["result"] = client.rpc("ping", {"x": 1}, t=2.0)
                except Exception as e:
                    slot["error"] = e
            t = threading.Thread(target=driver, daemon=True)
            t.start()

            # Block until the client has actually written the request.
            # By the time readline() returns, rpc() is past the write
            # and has registered its response queue.
            line = in_r_file.readline()
            self.assertTrue(line, "client never wrote a request")
            msg = json.loads(line)
            self.assertEqual(msg["method"], "ping")
            self.assertEqual(msg["params"], {"x": 1})

            write_response({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "result": {"ok": True, "echo": msg["params"]},
            })

            t.join(timeout=2.0)
            self.assertFalse(t.is_alive(), "rpc() never returned")
            self.assertNotIn("error", slot, f"unexpected error: {slot.get('error')}")
            self.assertEqual(slot["result"], {"ok": True, "echo": {"x": 1}})
        finally:
            cleanup()

    def test_error_response_raises_runtime_error(self):
        import threading

        client, in_r_file, write_response, cleanup = self._make_pipes_and_client()
        try:
            slot = {}
            def driver():
                try:
                    client.rpc("bad", {}, t=2.0)
                except Exception as e:
                    slot["error"] = e
            t = threading.Thread(target=driver, daemon=True)
            t.start()

            line = in_r_file.readline()
            msg = json.loads(line)
            write_response({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32600, "message": "bad request"},
            })

            t.join(timeout=2.0)
            self.assertFalse(t.is_alive(), "rpc() never returned")
            self.assertIsInstance(slot.get("error"), RuntimeError)
            err_text = str(slot["error"])
            self.assertIn("bad", err_text)
            self.assertIn("bad request", err_text)
        finally:
            cleanup()

    def test_notification_lands_on_drain_queue(self):
        # Notifications (no `id`) bypass the response queue entirely —
        # the reader puts them directly on `client.notifs` whether or
        # not anyone is waiting. No correlation race here, so we can
        # safely pre-stage the message onto the client's stdout pipe.
        import time

        client, _in_r_file, write_response, cleanup = self._make_pipes_and_client()
        try:
            write_response({
                "jsonrpc": "2.0",
                "method": "session.exited",
                "params": {"session": "s1", "sequence": 42},
            })

            # Deterministic poll — no sleep loops.
            deadline = time.monotonic() + 2.0
            notifs = []
            while time.monotonic() < deadline:
                notifs = client.drain_notifs()
                if notifs:
                    break
                time.sleep(0.01)
            self.assertEqual(len(notifs), 1, "notification never arrived on drain_notifs queue")
            self.assertEqual(notifs[0]["method"], "session.exited")
            self.assertEqual(notifs[0]["params"]["session"], "s1")
        finally:
            cleanup()


if __name__ == "__main__":
    unittest.main()
