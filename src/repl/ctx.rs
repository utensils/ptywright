//! Shared mutable state for the REPL.
//!
//! `ReplCtx` is the single source of truth threaded between the dispatcher
//! (which mutates `focus` / `adapters` / `history` in response to user
//! commands) and the TUI renderer (which reads them to draw the tab strip,
//! history pane, and footer). It is wrapped in an `Arc<Mutex<_>>` at the
//! call site so the snapshot pump and TUI thread can both touch it.
//!
//! Live screen snapshots have their own dedicated lock — see the snapshot
//! pump module — because they are written far more frequently than the
//! rest of `ReplCtx` and we want render reads to be cheap.

use std::collections::VecDeque;

/// One adapter the REPL is monitoring. Plugin name + last-known state label
/// keep the tab strip render self-contained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterTab {
    pub id: String,
    pub plugin: String,
    pub state_label: Option<String>,
}

/// What category of result a history entry represents — used by the TUI to
/// pick the glyph/colour in front of the line ("✓" / "↪" / "✗").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeKind {
    Ok,
    Json,
    Note,
    Error,
}

/// One line in the rolling REPL history pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub input: String,
    pub kind: OutcomeKind,
    /// Optional summary surfaced under the input (e.g. "wrote 18 bytes").
    pub detail: Option<String>,
}

/// REPL state shared between dispatch and TUI.
#[derive(Debug, Clone)]
pub struct ReplCtx {
    pub focus: Option<String>,
    pub adapters: Vec<AdapterTab>,
    pub history: VecDeque<HistoryEntry>,
    /// Maximum number of history entries kept in memory. Older entries are
    /// dropped from the front of the deque when this is exceeded.
    pub max_history: usize,
}

impl Default for ReplCtx {
    fn default() -> Self {
        Self {
            focus: None,
            adapters: Vec::new(),
            history: VecDeque::new(),
            max_history: 200,
        }
    }
}

impl ReplCtx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Find an adapter by id.
    pub fn adapter(&self, id: &str) -> Option<&AdapterTab> {
        self.adapters.iter().find(|tab| tab.id == id)
    }

    /// Find an adapter by id mutably.
    pub fn adapter_mut(&mut self, id: &str) -> Option<&mut AdapterTab> {
        self.adapters.iter_mut().find(|tab| tab.id == id)
    }

    /// Insert or update an adapter, keeping insertion order for fresh ids.
    pub fn upsert_adapter(&mut self, id: &str, plugin: &str) {
        if let Some(existing) = self.adapter_mut(id) {
            existing.plugin = plugin.to_string();
            return;
        }
        self.adapters.push(AdapterTab {
            id: id.to_string(),
            plugin: plugin.to_string(),
            state_label: None,
        });
    }

    /// Remove an adapter and clear focus if it pointed at the removed id.
    /// Returns whether anything was removed.
    pub fn remove_adapter(&mut self, id: &str) -> bool {
        let before = self.adapters.len();
        self.adapters.retain(|tab| tab.id != id);
        if self.focus.as_deref() == Some(id) {
            self.focus = self.adapters.last().map(|tab| tab.id.clone());
        }
        self.adapters.len() != before
    }

    /// Update the cached state label for an adapter, if known.
    pub fn set_state_label(&mut self, id: &str, label: Option<String>) {
        if let Some(tab) = self.adapter_mut(id) {
            tab.state_label = label;
        }
    }

    /// Record one history entry, trimming the front of the deque to honor
    /// `max_history`.
    pub fn record(&mut self, entry: HistoryEntry) {
        self.history.push_back(entry);
        while self.history.len() > self.max_history {
            self.history.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_adds_then_updates() {
        let mut ctx = ReplCtx::new();
        ctx.upsert_adapter("e1", "claude-code");
        assert_eq!(ctx.adapters.len(), 1);
        ctx.upsert_adapter("e1", "claude-code"); // idempotent
        assert_eq!(ctx.adapters.len(), 1);
        ctx.upsert_adapter("e2", "claude-code");
        assert_eq!(ctx.adapters.len(), 2);
    }

    #[test]
    fn remove_falls_back_focus_to_last_remaining() {
        let mut ctx = ReplCtx::new();
        ctx.upsert_adapter("e1", "x");
        ctx.upsert_adapter("e2", "x");
        ctx.focus = Some("e2".into());
        assert!(ctx.remove_adapter("e2"));
        assert_eq!(ctx.focus.as_deref(), Some("e1"));
        assert!(ctx.remove_adapter("e1"));
        assert!(ctx.focus.is_none());
    }

    #[test]
    fn record_trims_to_max_history() {
        let mut ctx = ReplCtx::new();
        ctx.max_history = 3;
        for n in 0..5 {
            ctx.record(HistoryEntry {
                input: format!("cmd-{n}"),
                kind: OutcomeKind::Ok,
                detail: None,
            });
        }
        assert_eq!(ctx.history.len(), 3);
        assert_eq!(ctx.history.front().unwrap().input, "cmd-2");
        assert_eq!(ctx.history.back().unwrap().input, "cmd-4");
    }

    #[test]
    fn state_label_setter_is_noop_for_unknown_adapter() {
        let mut ctx = ReplCtx::new();
        ctx.set_state_label("nope", Some("busy".into()));
        assert!(ctx.adapters.is_empty());
    }
}
