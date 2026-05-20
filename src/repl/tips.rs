//! Rotating startup tips + the long-form `:tips` guide.
//!
//! Surfacing one focused idiom per session beats a wall of help text at
//! launch — operators see something different each run and pick up the
//! language naturally. The longer guide stays opt-in through the `:tips`
//! meta command for when a refresher is wanted.
//!
//! Picking from the rotating set is deterministic per-session: the
//! selection is keyed off the wall clock at startup, so the same line
//! never appears twice in a row but tests can still seed a specific
//! index via [`rotating_tip_for`].

use std::time::{SystemTime, UNIX_EPOCH};

/// One-liners surfaced under the startup banner. Each is meant to fit
/// on one terminal row at typical widths (≤ 110 chars) and demonstrate
/// a single self-contained idiom the operator can copy-paste verbatim.
/// Add new entries freely — there is no ordering requirement.
const ROTATING_TIPS: &[&str] = &[
    "type `plugins()` to see what you can spawn, then `session.spawn(\"claude-code\")` to drive one",
    "press Tab inside `session.spawn(` to pick a plugin · Shift-Tab steps back",
    "`view()` renders the focused PTY inline — same styling the agent itself sees",
    "the two-call loop is `send.text \"hello\"` then `wait(matches \"^❯ \")`",
    "`turn{ \"send_prompt\", prompt = \"hi\", wait = matches \"^❯ \" }` is the atomic send-then-wait",
    "trailing tables are Lua kwargs — `session.spawn{ \"name\", rows = 24 }`",
    "`:tabs` for a one-line adapter summary · `:focus <id>` to switch",
    "`local s = screen.snapshot()` captures the screen so you can poke at `s.cursor` etc.",
    "the full Lua stdlib is available — `os.date()`, `string.format(...)`, `for i = 1, 5 do ... end`",
    "`:rpc <method> {json}` is the raw JSON-RPC pass-through for anything the bindings don't cover",
    "`:notifications off` silences `session.changed` chatter when you want a quiet prompt",
    "multi-line input just works: open a `do` / `function` / `{` and the prompt switches to `...`",
];

/// Curated tour rendered by the `:tips` meta command. Kept verbatim
/// (no string interpolation) so the layout is stable and easy to
/// review in diffs.
const LONG_GUIDE: &str = "\
The REPL evaluates real Lua 5.4. Every line below is a function call;\n\
the trailing-table sugar (`{ ... }`) is Lua's call-with-kwargs idiom.\n\
\n\
1. list and spawn:\n\
   plugins()                              list built-in TUIs\n\
   session.spawn(\"claude-code\")           spawn an adapter (focus follows)\n\
   session.spawn{ \"claude-code\", rows = 24 }\n\
                                          same call, with kwargs\n\
\n\
2. drive the focused adapter:\n\
   send.text \"explain this repo\"          send a prompt (string sugar)\n\
   send.key \"enter\"                       send a single key\n\
   wait(matches \"^❯ \")                    block until a regex hits\n\
   wait(screen_stable(ms(250)))           wait for the screen to settle\n\
   view()                                 render the focused PTY inline\n\
\n\
3. atomic send + wait:\n\
   turn{ \"send_prompt\", prompt = \"hi\",\n\
         wait = matches \"^❯ \", timeout = s(10) }\n\
                                          one round-trip · auto-correlated\n\
\n\
4. inspect and debug:\n\
   state()                                re-classify the focused adapter\n\
   transcript.snapshot()                  dump (redacted) transcript\n\
   inspect()                              raw adapter.inspect dump\n\
\n\
5. tabs and focus:\n\
   session.list()                         adapters known to this REPL\n\
   :tabs                                  same, terse one-line view\n\
   :focus <id>                            switch focus\n\
   session.attach \"id\"  /  \"all\"          adopt a server-side adapter\n\
\n\
Quality-of-life:\n\
   - Tab cycles completions · Shift-Tab steps back\n\
   - Multi-line input: opening `do` / `function` / `{` extends the prompt\n\
   - `:notifications off` silences `session.changed` events\n\
   - `:rpc <method> {json}` for raw JSON-RPC pass-through\n\
   - `:help` shows the full command reference\n";

/// Pick a tip using the wall clock as the seed. Two REPL sessions
/// launched in the same second see the same tip — that's fine; the
/// goal is variety across sessions, not uniqueness within a second.
pub fn rotating_tip() -> &'static str {
    rotating_tip_for(seed_from_clock())
}

/// Long-form guide rendered by `:tips`.
pub fn long_guide() -> &'static str {
    LONG_GUIDE
}

/// Deterministic accessor for tests. Returns the rotating tip that
/// would be selected for the given seed, or an empty string if the
/// rotating set is empty.
pub fn rotating_tip_for(seed: u64) -> &'static str {
    if ROTATING_TIPS.is_empty() {
        return "";
    }
    let idx = (seed as usize) % ROTATING_TIPS.len();
    ROTATING_TIPS[idx]
}

fn seed_from_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_set_is_non_empty_and_one_line_each() {
        assert!(!ROTATING_TIPS.is_empty(), "ROTATING_TIPS must not be empty");
        for tip in ROTATING_TIPS {
            assert!(
                !tip.contains('\n'),
                "rotating tip must fit on one line, got: {tip}"
            );
            assert!(
                tip.chars().count() <= 120,
                "rotating tip too long ({} chars): {tip}",
                tip.chars().count(),
            );
        }
    }

    #[test]
    fn rotating_tip_for_is_deterministic_per_seed() {
        let a = rotating_tip_for(7);
        let b = rotating_tip_for(7);
        assert_eq!(a, b, "same seed must give same tip");
    }

    #[test]
    fn rotating_tip_for_cycles_through_set() {
        // Walking N consecutive seeds must cover every entry in the set
        // (the modulo guarantees this when N == ROTATING_TIPS.len()).
        let mut seen: std::collections::HashSet<&'static str> = Default::default();
        for seed in 0..(ROTATING_TIPS.len() as u64) {
            seen.insert(rotating_tip_for(seed));
        }
        assert_eq!(
            seen.len(),
            ROTATING_TIPS.len(),
            "every tip must be reachable",
        );
    }

    #[test]
    fn long_guide_mentions_each_load_bearing_idiom() {
        // `view()` is the shorter alias the guide leads with; the
        // longer `screen.snapshot()` is hinted at in the rotating
        // tip set instead so the curated tour stays scannable.
        let g = long_guide();
        for needle in [
            "plugins()",
            "session.spawn",
            "send.text",
            "wait(matches",
            "turn{",
            "view()",
            "transcript.snapshot",
            ":tabs",
            ":focus",
            ":rpc",
            ":help",
        ] {
            assert!(g.contains(needle), "guide should mention `{needle}`: {g}");
        }
    }
}
