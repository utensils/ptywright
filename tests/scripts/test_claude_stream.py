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


class PromptEditableAnchorTests(unittest.TestCase):
    """Submission verification must distinguish a submitted prompt echo
    from prompt text still sitting in Claude's editable input box."""

    def test_detects_prompt_still_editable_on_last_body_row(self):
        body = "\n".join([
            " ▐▛███▜▌   Claude Code v2.1.145",
            "▝▜█████▛▘  Sonnet 4.6 · Claude API",
            "",
            "─" * 80,
            "❯\u00a0Explore project and summerize it.",
        ])
        self.assertTrue(
            CS._prompt_anchor_looks_editable(
                body,
                "⏵⏵ auto mode on (shift+tab to cycle)",
                "Explore project and summerize it.",
            )
        )

    def test_activity_indicator_means_prompt_was_submitted(self):
        body = "\n".join([
            "❯\u00a0Explore project and summerize it.",
            "",
            "✽ Simmering… (5s · ↓ 215 tokens · thinking)",
        ])
        self.assertFalse(
            CS._prompt_anchor_looks_editable(
                body,
                "",
                "Explore project and summerize it.",
            )
        )

    def test_answer_line_after_prompt_is_not_editable_prompt(self):
        body = "\n".join([
            "❯\u00a0Explore project and summerize it.",
            "",
            "⏺ I'll explore the project structure.",
        ])
        self.assertFalse(
            CS._prompt_anchor_looks_editable(
                body,
                "",
                "Explore project and summerize it.",
            )
        )


class DumpAnswerRegionFromTranscriptTests(unittest.TestCase):
    """Primary fallback at completion — pulls the answer region out
    of the full PTY scrollback. The visible body is bounded (alt-screen
    snapshot of ~60 rows); the transcript isn't. When Claude's tool
    output and prose scroll past the visible body, this fallback is the
    only thing that surfaces the actual reply."""

    def _make_stream(self) -> "CS.Stream":
        stream = CS.Stream.__new__(CS.Stream)
        stream._closed = False
        stream._last_alive_at = 0.0
        # Fake an RPC client that returns canned transcript text.
        stream.client = mock.Mock()
        stream.aid = "e-test"
        stream._transcript_baseline = 0
        stream._submitted_prompt = ""
        return stream

    def test_dumps_answer_region_when_body_scrolled_past(self):
        stream = self._make_stream()
        # The full transcript is what the user "would see" if they
        # could scroll back. Pretend the visible body only shows the
        # last few rows by the time we're called.
        transcript_text = "\n".join([
            "Welcome chrome",
            "",
            "❯ List exactly three .rs files.",
            "",
            "⏺ I'll find three .rs files.",
            "Reading project structure… (ctrl+o to expand)",  # in-flight chrome
            "⏺ Glob(**/*.rs)",
            "  src/lib.rs",
            "  src/main.rs",
            "  src/action.rs",
            "",
            "✻ Brewed for 2s",
            "",
            "❯",
        ])
        stream._transcript_baseline = 0
        stream._submitted_prompt = "List exactly three .rs files."
        stream.client.rpc.return_value = {"text": transcript_text}

        captured = io.StringIO()
        with mock.patch.object(sys, "stdout", captured):
            printed = stream._dump_answer_region_from_transcript()
        out = captured.getvalue()

        self.assertTrue(printed)
        self.assertIn("⏺ I'll find three .rs files.", out)
        self.assertIn("⏺ Glob(**/*.rs)", out)
        self.assertIn("src/lib.rs", out)
        self.assertIn("✻ Brewed for 2s", out)
        # In-flight chrome line is filtered.
        self.assertNotIn("Reading project structure…", out)
        # Welcome chrome above the prompt echo is filtered.
        self.assertNotIn("Welcome chrome", out)

    def test_returns_false_when_transcript_empty(self):
        stream = self._make_stream()
        stream._transcript_baseline = 100
        stream._submitted_prompt = "anything"
        stream.client.rpc.return_value = {"text": "x" * 100}  # baseline eats it all

        captured = io.StringIO()
        with mock.patch.object(sys, "stdout", captured):
            printed = stream._dump_answer_region_from_transcript()
        self.assertFalse(printed)
        self.assertEqual(captured.getvalue(), "")

    def test_anchors_on_last_prompt_occurrence(self):
        # Bracketed paste echoes the prompt text more than once during
        # the input box's stages. The anchor must be the LAST occurrence
        # so the answer region doesn't include the in-progress echoes.
        stream = self._make_stream()
        transcript_text = "\n".join([
            "❯ summarize",  # mid-paste partial echo
            "❯ summarize the project",  # full echo (the anchor)
            "",
            "⏺ Project summary:",
            "- Rust CLI for PTY automation",
            "✻ Brewed for 1s",
        ])
        stream._transcript_baseline = 0
        stream._submitted_prompt = "summarize the project"
        stream.client.rpc.return_value = {"text": transcript_text}

        captured = io.StringIO()
        with mock.patch.object(sys, "stdout", captured):
            stream._dump_answer_region_from_transcript()
        out = captured.getvalue()

        self.assertIn("⏺ Project summary:", out)
        self.assertIn("Rust CLI for PTY automation", out)
        self.assertIn("✻ Brewed for 1s", out)


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

    def test_exit_code_constants_are_distinct(self):
        # Wrappers (claudette, multi-account drivers) discriminate
        # failure modes on these. Catch any accidental collision.
        codes = {
            CS.Stream.EXIT_OK,
            CS.Stream.EXIT_GENERIC,
            CS.Stream.EXIT_RATE_LIMIT,
            CS.Stream.EXIT_QUOTA,
            CS.Stream.EXIT_STUCK,
        }
        self.assertEqual(len(codes), 5,
                         "exit code constants must be pairwise distinct")
        self.assertEqual(CS.Stream.EXIT_OK, 0)


class CompletionMarkerLatchTests(unittest.TestCase):
    """`_is_completion_marker` must distinguish the real end-of-turn
    line (`✻ Brewed for 2s`) from an in-flight spinner frame whose
    parenthesized tail happens to contain ` for <text>` (e.g.
    `✻ Pondering… (5s · ↓ 12 tokens · for context)`). Without the
    ellipsis guard, latching on a spinner frame would mask real
    subsequent hangs as `EXIT_OK`.
    """

    def test_real_marker_matches(self):
        self.assertTrue(CS._is_completion_marker("✻ Brewed for 2s"))
        self.assertTrue(CS._is_completion_marker("✻ Worked for 45s"))
        self.assertTrue(CS._is_completion_marker("✻ Cogitated for 1m"))

    def test_spinner_frame_with_for_in_tail_does_not_match(self):
        # Copilot review regression — must NOT latch on spinner frames.
        self.assertFalse(CS._is_completion_marker(
            "✻ Pondering… (5s · ↓ 12 tokens · for context)"))
        self.assertFalse(CS._is_completion_marker("✻ Working…"))
        self.assertFalse(CS._is_completion_marker(
            "✻ Brewing… (12s · ↑ 217 tokens · esc to interrupt)"))

    def test_non_marker_glyph_lines_do_not_match(self):
        self.assertFalse(CS._is_completion_marker("⏺ Hi there!"))
        self.assertFalse(CS._is_completion_marker("> just text for fun"))


class ErrorMetadataAccessorTests(unittest.TestCase):
    """`_last_error_kind` and `_format_error_detail` shape the rate-limit
    / quota fail-fast path. Wrong shape => wrong exit code => wrappers
    can't swap accounts."""

    def _make_stream(self):
        stream = CS.Stream.__new__(CS.Stream)
        stream._last_metadata = None
        return stream

    def test_kind_none_when_no_metadata(self):
        s = self._make_stream()
        self.assertIsNone(s._last_error_kind())
        self.assertEqual(s._format_error_detail(), "")

    def test_kind_rate_limit_with_retry(self):
        s = self._make_stream()
        s._last_metadata = {"error": {
            "kind": "rate_limit",
            "message": "Rate limit reached. Please try again in 42 seconds.",
            "retry_after_s": 42,
        }}
        self.assertEqual(s._last_error_kind(), "rate_limit")
        detail = s._format_error_detail()
        self.assertIn("Rate limit reached", detail)
        self.assertIn("retry_after=42s", detail)

    def test_kind_quota_no_retry(self):
        s = self._make_stream()
        s._last_metadata = {"error": {
            "kind": "quota",
            "message": "Credit balance is too low",
        }}
        self.assertEqual(s._last_error_kind(), "quota")
        detail = s._format_error_detail()
        self.assertIn("Credit balance is too low", detail)
        self.assertNotIn("retry_after", detail)

    def test_malformed_metadata_returns_none(self):
        s = self._make_stream()
        # error subtree is not a dict — fixture parsing edge case.
        s._last_metadata = {"error": "boom"}
        self.assertIsNone(s._last_error_kind())
        self.assertEqual(s._format_error_detail(), "")


class IdleGrowthDetectorTests(unittest.TestCase):
    """Stuck detector samples transcript length at most once per second
    and reports seconds-since-last-growth. Verifies the bookkeeping is
    monotonic and the rate-limit doesn't drop growth signals."""

    def _make_stream(self):
        stream = CS.Stream.__new__(CS.Stream)
        stream.client = mock.Mock()
        stream.aid = "e-test"
        stream._growth_check_at = 0.0
        stream._growth_last_len = None
        stream._growth_last_grew_at = None
        return stream

    def _set_transcript(self, stream, text):
        stream.client.rpc.return_value = {"text": text}

    def test_first_sample_seeds_tracker(self):
        s = self._make_stream()
        self._set_transcript(s, "x" * 100)
        # First call seeds — duration since growth is 0.
        self.assertEqual(s._check_idle_growth(), 0.0)
        self.assertEqual(s._growth_last_len, 100)
        self.assertIsNotNone(s._growth_last_grew_at)

    def test_growth_above_noise_floor_resets_timer(self):
        import time as _time
        s = self._make_stream()
        self._set_transcript(s, "x" * 1000)
        s._check_idle_growth()  # seed
        # Force the next sample to bypass the 1.0s rate-limit and feed
        # a burst above PASTE_REACTION_BYTES (256). The timer resets.
        s._growth_check_at = _time.monotonic() - 2.0
        s._growth_last_grew_at = _time.monotonic() - 5.0
        self._set_transcript(s, "x" * (1000 + CS.PASTE_REACTION_BYTES + 10))
        result = s._check_idle_growth()
        self.assertEqual(result, 0.0)
        self.assertEqual(s._growth_last_len, 1000 + CS.PASTE_REACTION_BYTES + 10)

    def test_sub_threshold_trickle_does_NOT_reset_timer(self):
        """The core regression test for the original hang. Status-bar
        token-counter updates produce sub-256-byte growth that must
        accumulate against the stuck timer instead of silently
        resetting it on every poll."""
        import time as _time
        s = self._make_stream()
        self._set_transcript(s, "x" * 1000)
        s._check_idle_growth()  # seed; baseline = 1000
        seed_grew_at = s._growth_last_grew_at
        seed_baseline = s._growth_last_len
        # Pretend a status-bar tick added 50 bytes — below the 256
        # threshold. The baseline must NOT advance, and the timer
        # must NOT reset.
        s._growth_check_at = _time.monotonic() - 2.0
        s._growth_last_grew_at = seed_grew_at - 10.0
        self._set_transcript(s, "x" * 1050)
        delta = s._check_idle_growth()
        self.assertGreaterEqual(delta, 9.5,
            "sub-threshold growth must not reset _growth_last_grew_at")
        self.assertEqual(s._growth_last_len, seed_baseline,
            "_growth_last_len must NOT advance on sub-threshold growth — "
            "otherwise trickle silently rebases the comparison and the "
            "next 256-byte check never triggers")

    def test_trickle_accumulates_above_threshold_then_resets(self):
        """A series of sub-threshold growths totalling >=256 bytes IS
        real progress and must reset the timer once the accumulated
        delta crosses the noise floor. Without this, a slow but
        legitimate model would be wrongly classified as stuck."""
        import time as _time
        s = self._make_stream()
        self._set_transcript(s, "x" * 1000)
        s._check_idle_growth()  # seed
        # Three small bursts of 100 bytes each (300 total > 256).
        for grown_to in (1100, 1200, 1300):
            s._growth_check_at = _time.monotonic() - 2.0
            self._set_transcript(s, "x" * grown_to)
            delta = s._check_idle_growth()
            if grown_to < 1000 + CS.PASTE_REACTION_BYTES:
                # Still below the accumulated threshold — timer must
                # still be running.
                self.assertGreater(delta, 0.0)
            else:
                # Crossed the threshold — timer resets.
                self.assertEqual(delta, 0.0)
                self.assertEqual(s._growth_last_len, grown_to)

    def test_no_growth_accumulates_duration(self):
        import time as _time
        s = self._make_stream()
        self._set_transcript(s, "x" * 100)
        s._check_idle_growth()  # seed
        # Same length, but the check is rate-limited so the RPC won't
        # fire. The function falls back to "now - last_grew_at".
        seed_grew_at = s._growth_last_grew_at
        # Move the cached grew_at back to simulate elapsed time.
        s._growth_last_grew_at = seed_grew_at - 10.0
        # Under the 1.0s rate-limit, no fresh RPC; cached delta returned.
        delta = s._check_idle_growth()
        self.assertGreaterEqual(delta, 9.5)

    def test_rpc_failure_does_not_corrupt_timer(self):
        import time as _time
        s = self._make_stream()
        # Seed first.
        self._set_transcript(s, "x" * 100)
        s._check_idle_growth()
        # Now force the rate-limit to expire AND make the RPC raise.
        s._growth_check_at = _time.monotonic() - 2.0
        s.client.rpc.side_effect = RuntimeError("server gone")
        delta = s._check_idle_growth()
        # Must still return a non-None duration, not crash. State left
        # untouched so the next successful sample can continue.
        self.assertIsNotNone(delta)


class ChromeSpinnerGlyphSetTests(unittest.TestCase):
    """Reminder test: the Python `_CHROME_SPINNER_GLYPHS` char class
    must be a SUPERSET of the Lua `SPINNER_GLYPHS` table.

    This test does NOT auto-discover the Lua set — `LUA_SPINNERS` below
    is hand-mirrored from `plugins/claude-code/main.lua`. If a new
    glyph is added to the Lua table, this test won't catch it on its
    own. Maintainers updating `SPINNER_GLYPHS` in Lua must also
    propagate the addition to the Python char class AND extend this
    LUA_SPINNERS list — failure to do so means the streaming loop
    will leak the new spinner frames as content even though the
    classifier filters them. Treat the list as a checklist, not a
    safety net."""

    LUA_SPINNERS = ["✶", "✻", "✺", "✦", "·", "•",
                    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]

    def test_python_chrome_set_covers_lua_spinner_set(self):
        for glyph in self.LUA_SPINNERS:
            self.assertIn(
                glyph, CS._CHROME_SPINNER_GLYPHS,
                f"Lua spinner glyph `{glyph}` (from LUA_SPINNERS in this "
                "test) is not in _CHROME_SPINNER_GLYPHS. Update the Python "
                "char class to cover it."
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

            # Bounded poll with a short sleep — `time.sleep(0.01)` keeps
            # the loop from spinning while the daemon reader thread
            # delivers the notification. The 2.0s deadline makes this
            # deterministic on every host (any slower than that is a
            # real failure, not flake).
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
