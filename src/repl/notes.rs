//! Structured `↳` result notes for the REPL log.
//!
//! Each dispatched command produces a [`Note`] — a short primary line of
//! dim text plus optional accent-colored badges — that the TUI renders
//! under the `pty>` prompt. This matches the polished `OperatorHome.vue`
//! mockup on the docs site (concise notes like
//! `spawned pid 48211 · 80×24 · alt-screen`,
//! `stable after 412ms · seq 14`, `matched · row 19 · [turn complete]`)
//! and replaces the old behavior of dumping the full RPC JSON response
//! as the `↳` line.
//!
//! Helpers in this module read the raw JSON response from the generic
//! `adapter.*` RPC surface and project the fields the operator actually
//! wants to see. The functions are deliberately plugin-agnostic: they
//! look at common fields like `state.state`, `state.sequence`, and the
//! `cursor` / `size` shapes that every plugin's `ScreenSnapshot`
//! produces. Nothing here knows about claude-code specifically.

use std::time::Duration;

use nu_ansi_term::{Color, Style};
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// One `↳` summary rendered under a command in the REPL log.
///
/// `primary` is shown in dim text right after the `↳` glyph; `badges`
/// are accent-colored chips appended after `primary` and used for
/// noteworthy lifecycle transitions (e.g. `turn complete`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Note {
    pub primary: String,
    pub badges: Vec<String>,
}

impl Note {
    /// Build a note from a single primary line.
    pub fn line(primary: impl Into<String>) -> Self {
        Self {
            primary: primary.into(),
            badges: Vec::new(),
        }
    }

    /// Append a badge to the note.
    pub fn with_badge(mut self, badge: impl Into<String>) -> Self {
        self.badges.push(badge.into());
        self
    }

    /// Render the note as an ANSI-styled string, ready for the TUI to
    /// print on its own line. Includes the leading `↳` glyph and the
    /// surrounding indent so callers don't have to remember the
    /// formatting contract.
    pub fn render(&self) -> String {
        let mut out = format!(
            "  {} {}",
            Color::DarkGray.paint("↳"),
            Style::new().dimmed().paint(&self.primary)
        );
        for badge in &self.badges {
            out.push(' ');
            // Amber on dim background to match the mockup's
            // `turn complete` chip without depending on terminal
            // background color support.
            let chip = format!(" {badge} ");
            out.push_str(&Color::Fixed(214).reverse().paint(chip).to_string());
        }
        out
    }

    /// Convert the note into a single ratatui `Line` ready to drop into a
    /// `Paragraph` or list widget. Uses the same `↳ <dim primary> [ amber
    /// chip ]…` visual language as [`Self::render`], but with ratatui's
    /// styled spans instead of inline ANSI bytes.
    pub fn to_line(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(3 + self.badges.len() * 2);
        spans.push(Span::raw("  "));
        spans.push(Span::styled("↳", RStyle::default().fg(RColor::DarkGray)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            self.primary.clone(),
            RStyle::default().add_modifier(RModifier::DIM),
        ));
        for badge in &self.badges {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!(" {badge} "),
                RStyle::default().fg(RColor::Black).bg(RColor::Indexed(214)),
            ));
        }
        Line::from(spans)
    }
}

// ---- per-command formatters --------------------------------------------

/// `↳` note for `session.spawn(...)`. Reads `adapter`, `plugin`, the
/// initial `state.state` label, and (when present) `size.cols × size.rows`
/// from the `adapter.start` response. The PID is included when the
/// server returns it; some plugins or transports may omit it.
pub fn spawned(response: &Value) -> Note {
    let mut parts: Vec<String> = Vec::new();
    if let Some(id) = response.get("adapter").and_then(Value::as_str) {
        parts.push(id.to_string());
    }
    if let Some(plugin) = response.get("plugin").and_then(Value::as_str) {
        parts.push(plugin.to_string());
    }
    if let Some(dims) = format_dims_from_state(response.get("state")) {
        parts.push(dims);
    }
    if response
        .get("state")
        .and_then(|s| s.get("metadata"))
        .and_then(|m| m.get("alternate_screen"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("alt-screen".to_string());
    }
    let primary = if parts.is_empty() {
        "spawned".to_string()
    } else {
        format!("spawned · {}", parts.join(" · "))
    };
    Note::line(primary)
}

/// `↳` note for `session.close(...)`. Just confirms the id we asked the
/// server to close. The server's response shape varies by plugin so we
/// don't try to parse it.
pub fn closed(adapter: &str) -> Note {
    Note::line(format!("closed · {adapter}"))
}

/// `↳` note for `send.text("…")`. Reports the byte count of the prompt
/// (a useful sanity check for paste-vs-typed and accidental \n) and the
/// resulting plugin state name when the response carries it.
pub fn send_text(text: &str, response: &Value) -> Note {
    let bytes = text.len();
    let state_suffix = state_label_suffix(response);
    Note::line(format!("wrote {bytes} bytes{state_suffix}"))
}

/// `↳` note for `send.key("…")`. Mirrors [`send_text`] but reports the
/// key name instead of a byte count.
pub fn send_key(key: &str, response: &Value) -> Note {
    let state_suffix = state_label_suffix(response);
    Note::line(format!("sent key {key}{state_suffix}"))
}

/// `↳` note for `send.intent("name", …)`. Plugins return a wide variety
/// of response shapes here so the formatter is conservative — just the
/// intent name and the resulting state when it's available.
pub fn send_intent(intent: &str, response: &Value) -> Note {
    let state_suffix = state_label_suffix(response);
    Note::line(format!("intent {intent}{state_suffix}"))
}

/// `↳` note for `wait(matches(r"…"))`. Times the call and reads the
/// matched row (when the server returns it) so the operator can correlate
/// with what they see on screen.
pub fn wait_match(elapsed: Duration, response: &Value) -> Note {
    let elapsed_ms = elapsed.as_millis();
    let row = response
        .get("matched")
        .and_then(|m| m.get("row"))
        .and_then(Value::as_u64);
    let seq = response
        .get("state")
        .and_then(|s| s.get("sequence"))
        .and_then(Value::as_u64);
    let row_part = row.map(|r| format!(" · row {r}")).unwrap_or_default();
    let seq_part = seq.map(|s| format!(" · seq {s}")).unwrap_or_default();
    Note::line(format!("matched after {elapsed_ms}ms{row_part}{seq_part}"))
}

/// `↳` note for `wait(screen_stable(...))`. Times the call and includes
/// the resulting sequence so a follow-up `view()` reader can confirm
/// they're looking at the same frame the wait settled on.
pub fn wait_stable(elapsed: Duration, response: &Value) -> Note {
    let elapsed_ms = elapsed.as_millis();
    let seq = response
        .get("state")
        .and_then(|s| s.get("sequence"))
        .and_then(Value::as_u64);
    let seq_part = seq.map(|s| format!(" · seq {s}")).unwrap_or_default();
    Note::line(format!("stable after {elapsed_ms}ms{seq_part}"))
}

/// `↳` note for `turn("send.…", wait=…)`. Same shape as [`wait_match`]
/// but always carries the `turn complete` accent badge so the operator
/// gets the visual confirmation cue the mockup features.
pub fn turn_complete(elapsed: Duration, response: &Value) -> Note {
    let elapsed_ms = elapsed.as_millis();
    let row = response
        .get("matched")
        .and_then(|m| m.get("row"))
        .and_then(Value::as_u64);
    let row_part = row.map(|r| format!(" · row {r}")).unwrap_or_default();
    Note::line(format!("turn completed in {elapsed_ms}ms{row_part}")).with_badge("turn complete")
}

/// `↳` note for `transcript.snapshot(redact=…)`. Reports the byte size
/// of the captured transcript and, when the server reports it, how many
/// redaction patterns matched. The transcript content itself is too
/// noisy to dump inline — the operator can pipe through `:rpc` if they
/// want the raw bytes.
pub fn transcript(response: &Value) -> Note {
    let transcript_text = response
        .get("transcript")
        .map(|t| serde_json::to_string(t).unwrap_or_default())
        .unwrap_or_default();
    let bytes = transcript_text.len();
    let redacted = response
        .get("redacted")
        .and_then(Value::as_u64)
        .or_else(|| {
            response
                .get("redactions")
                .and_then(Value::as_array)
                .map(|arr| arr.len() as u64)
        });
    let mut primary = format_bytes(bytes);
    if let Some(count) = redacted {
        primary.push_str(&format!(
            " · redacted {count} pattern{}",
            if count == 1 { "" } else { "s" }
        ));
    }
    Note::line(format!("transcript · {primary}"))
}

// ---- helpers -----------------------------------------------------------

/// Build the trailing ` · state: <name>` suffix used by the send/intent
/// formatters when the response carries a recognizable state label. The
/// suffix includes a leading separator so callers can splice it
/// directly into their primary text.
fn state_label_suffix(response: &Value) -> String {
    let label = response
        .get("state")
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str);
    match label {
        Some(name) if !name.is_empty() => format!(" · state: {name}"),
        _ => String::new(),
    }
}

/// Extract `<cols>×<rows>` from a state object's `size` field, when the
/// plugin's `ExtensionStateSnapshot` includes it. Returns `None` when
/// either dimension is missing so the formatter can omit the segment
/// rather than render `0×0`.
fn format_dims_from_state(state: Option<&Value>) -> Option<String> {
    let state = state?;
    let size = state.get("size")?;
    let cols = size.get("cols").and_then(Value::as_u64)?;
    let rows = size.get("rows").and_then(Value::as_u64)?;
    Some(format!("{cols}×{rows}"))
}

/// Human-friendly byte count: `512 B`, `1.2 KiB`, `3.4 MiB`. Used by
/// the transcript formatter and intentionally kept tiny — no need for a
/// full units crate for one place.
fn format_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let n = bytes as f64;
    if n >= MIB {
        format!("{:.1} MiB", n / MIB)
    } else if n >= KIB {
        format!("{:.1} KiB", n / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn note_with_badge_appends_in_order() {
        let n = Note::line("turn completed in 412ms").with_badge("turn complete");
        assert_eq!(n.primary, "turn completed in 412ms");
        assert_eq!(n.badges, vec!["turn complete".to_string()]);
    }

    #[test]
    fn note_render_includes_glyph_and_primary_text() {
        let rendered = Note::line("wrote 5 bytes").render();
        // The glyph and the primary text both have to surface in the
        // ANSI output — without them the operator sees nothing useful.
        assert!(rendered.contains("↳"));
        assert!(rendered.contains("wrote 5 bytes"));
    }

    #[test]
    fn spawned_lists_adapter_plugin_and_dims_when_present() {
        let resp = json!({
            "adapter": "e1",
            "plugin": "claude-code",
            "state": {
                "size": { "cols": 80, "rows": 24 },
                "metadata": { "alternate_screen": false }
            }
        });
        let note = spawned(&resp);
        assert!(note.primary.contains("e1"));
        assert!(note.primary.contains("claude-code"));
        assert!(note.primary.contains("80×24"));
        assert!(!note.primary.contains("alt-screen"));
    }

    #[test]
    fn spawned_includes_alt_screen_flag_when_state_reports_it() {
        let resp = json!({
            "adapter": "e1",
            "plugin": "p",
            "state": {
                "size": { "cols": 200, "rows": 60 },
                "metadata": { "alternate_screen": true }
            }
        });
        assert!(spawned(&resp).primary.contains("alt-screen"));
    }

    #[test]
    fn spawned_degrades_gracefully_when_state_missing() {
        let resp = json!({ "adapter": "e1", "plugin": "p" });
        let note = spawned(&resp);
        assert!(note.primary.contains("e1"));
        assert!(note.primary.contains("p"));
        // No size / no panic.
        assert!(!note.primary.contains('×'));
    }

    #[test]
    fn send_text_reports_byte_count_and_state() {
        let resp = json!({
            "state": { "state": "thinking", "sequence": 5 }
        });
        let note = send_text("hello", &resp);
        assert!(note.primary.contains("wrote 5 bytes"));
        assert!(note.primary.contains("state: thinking"));
    }

    #[test]
    fn send_text_omits_state_when_response_is_silent() {
        let note = send_text("hi", &json!({}));
        assert_eq!(note.primary, "wrote 2 bytes");
    }

    #[test]
    fn send_key_reports_key_name() {
        let note = send_key("Enter", &json!({"state": {"state": "ready"}}));
        assert!(note.primary.contains("sent key Enter"));
        assert!(note.primary.contains("state: ready"));
    }

    #[test]
    fn wait_match_includes_elapsed_and_row() {
        let resp = json!({
            "state": { "sequence": 14 },
            "matched": { "row": 19 }
        });
        let note = wait_match(Duration::from_millis(412), &resp);
        assert!(note.primary.contains("matched after 412ms"));
        assert!(note.primary.contains("row 19"));
        assert!(note.primary.contains("seq 14"));
    }

    #[test]
    fn wait_stable_includes_elapsed_and_sequence() {
        let resp = json!({ "state": { "sequence": 14 } });
        let note = wait_stable(Duration::from_millis(412), &resp);
        assert!(note.primary.contains("stable after 412ms"));
        assert!(note.primary.contains("seq 14"));
    }

    #[test]
    fn turn_complete_always_emits_the_badge() {
        let note = turn_complete(Duration::from_millis(800), &json!({}));
        assert_eq!(note.badges, vec!["turn complete".to_string()]);
        assert!(note.primary.contains("turn completed in 800ms"));
    }

    #[test]
    fn transcript_reports_bytes_and_redacted_count() {
        let resp = json!({
            "transcript": [{ "text": "hello" }],
            "redacted": 2
        });
        let note = transcript(&resp);
        assert!(note.primary.contains("redacted 2 patterns"));
    }

    #[test]
    fn transcript_singular_pattern_drops_the_s() {
        let resp = json!({
            "transcript": [],
            "redacted": 1
        });
        let note = transcript(&resp);
        assert!(note.primary.contains("redacted 1 pattern"));
        assert!(!note.primary.contains("patterns"));
    }

    #[test]
    fn format_bytes_picks_units_by_magnitude() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
