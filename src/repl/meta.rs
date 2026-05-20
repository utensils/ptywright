//! `:meta` command parsing and dispatch.
//!
//! Meta commands manage REPL-local state and a handful of escape hatches
//! that intentionally aren't Lua-callable:
//!
//! | Command | Purpose |
//! | --- | --- |
//! | `:tabs` | List adapter tabs known to this REPL |
//! | `:focus <id>` | Switch focus to a known adapter |
//! | `:live` | List adapters live on the server |
//! | `:attach <id\|all>` | Adopt server-side adapters into local tabs |
//! | `:notifications on\|off [adapters=…] [sessions=…]` | Subscribe filter |
//! | `:rpc <method> {json}` | Raw JSON-RPC escape hatch |
//! | `:help` | Inline help text |
//! | `:quit` (also `:q` / `:exit`) | Exit the REPL |
//!
//! Keeping these out of Lua keeps two invariants intact:
//!
//! 1. The Lua VM can't programmatically mutate REPL focus or the
//!    notification subscription — operators expect those to be explicit
//!    out-of-band controls, not the side-effect of a script.
//! 2. The raw `:rpc` JSON pass-through stays a literal-JSON UX —
//!    rewriting it into a Lua call would force operators to translate
//!    payload shapes by hand.

use std::time::Duration;

use serde_json::{Map, Value, json};

use super::ctx::ReplCtx;
use super::tips;
use super::transport::RpcClient;
use crate::error::{Error, Result};

/// What the read-eval-print loop should do after a meta command.
#[derive(Debug)]
pub enum MetaOutcome {
    /// Print one line as the "↳ …" result.
    Line(String),
    /// Print the JSON value (truncated for legibility).
    Json(Value),
    /// Render the inline help popup.
    ShowHelp(String),
    /// Render the longer rotating-tips guide.
    ShowTips(String),
    /// Render a styled terminal snapshot.
    Screen {
        adapter: String,
        snapshot: crate::screen::ScreenSnapshot,
    },
    /// Exit the REPL cleanly.
    Quit,
}

/// Parse and dispatch one meta command. `input` is the slice *after* the
/// leading `:` — `tui.rs` strips that before calling us.
pub fn dispatch(
    input: &str,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<MetaOutcome> {
    let mut parts = input.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let tail = parts.next().unwrap_or("").trim();
    match head {
        "help" => Ok(MetaOutcome::ShowHelp(help_text().to_string())),
        "tips" => Ok(MetaOutcome::ShowTips(tips::long_guide().to_string())),
        "quit" | "q" | "exit" => Ok(MetaOutcome::Quit),
        "tabs" => Ok(MetaOutcome::Line(format_tabs(ctx))),
        "focus" => focus(tail, ctx),
        "live" => list_live(client, timeout),
        "attach" => attach(tail, client, ctx, timeout),
        "notifications" => notifications(tail, client, timeout),
        "rpc" => rpc_passthrough(tail, client, timeout),
        other => Err(Error::Rpc(format!("unknown meta command `:{other}`"))),
    }
}

fn format_tabs(ctx: &ReplCtx) -> String {
    if ctx.adapters.is_empty() {
        return "(no adapters)".to_string();
    }
    ctx.adapters
        .iter()
        .map(|tab| {
            let mark = if ctx.focus.as_deref() == Some(tab.id.as_str()) {
                "*"
            } else {
                ""
            };
            format!("{mark}{}:{}", tab.id, tab.plugin)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn focus(id: &str, ctx: &mut ReplCtx) -> Result<MetaOutcome> {
    if id.is_empty() {
        return Err(Error::Rpc(":focus requires an adapter id".to_string()));
    }
    if ctx.adapter(id).is_none() {
        return Err(Error::Rpc(format!("unknown adapter `{id}`")));
    }
    ctx.focus = Some(id.to_string());
    Ok(MetaOutcome::Line(format!("focus → {id}")))
}

fn list_live(client: &RpcClient, timeout: Duration) -> Result<MetaOutcome> {
    let result = client.call("adapter.live", json!({}), timeout)?;
    Ok(MetaOutcome::Json(result))
}

fn attach(
    spec: &str,
    client: &RpcClient,
    ctx: &mut ReplCtx,
    timeout: Duration,
) -> Result<MetaOutcome> {
    if spec.is_empty() {
        return Err(Error::Rpc(
            ":attach requires an adapter id or `all`".to_string(),
        ));
    }
    if spec == "all" {
        let live = client.call("adapter.live", json!({}), timeout)?;
        let adapters = live
            .get("adapters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut attached = Vec::new();
        for entry in adapters {
            if entry
                .get("finished")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let id = match entry.get("adapter").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };
            let plugin = entry
                .get("plugin")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            ctx.upsert_adapter(&id, &plugin);
            attached.push(id);
        }
        if let Some(last) = attached.last() {
            ctx.focus = Some(last.clone());
        }
        let line = if attached.is_empty() {
            "(no live adapters on the server)".to_string()
        } else {
            format!(
                "attached {} adapter(s): {} (focus → {})",
                attached.len(),
                attached.join(", "),
                attached.last().cloned().unwrap_or_default(),
            )
        };
        return Ok(MetaOutcome::Line(line));
    }
    // Single-adapter attach: verify it exists, adopt it, render the screen.
    let state_resp = client.call("adapter.state", json!({ "adapter": spec }), timeout)?;
    let plugin = state_resp
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    ctx.upsert_adapter(spec, &plugin);
    if let Some(label) = state_resp
        .get("state")
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
    {
        ctx.set_state_label(spec, Some(label.to_string()));
    }
    ctx.focus = Some(spec.to_string());
    let snap_val = client.call(
        "adapter.snapshot",
        json!({ "adapter": spec, "redact": false }),
        timeout,
    )?;
    let snapshot: crate::screen::ScreenSnapshot = serde_json::from_value(snap_val)
        .map_err(|error| Error::Rpc(format!("decode adapter.snapshot: {error}")))?;
    Ok(MetaOutcome::Screen {
        adapter: spec.to_string(),
        snapshot,
    })
}

/// Parse the trailing args of `:notifications` and dispatch
/// `server.set_notifications`.
///
/// Supported forms:
/// ```text
/// :notifications on
/// :notifications off
/// :notifications on adapters=e1,e2
/// :notifications on sessions=s1,s2
/// :notifications on adapters=e1 sessions=s1
/// ```
///
/// Booleans accept `on`/`off`/`true`/`false`/`1`/`0` for symmetry with
/// the kwargs used elsewhere. Filter lists accept comma-separated ids with
/// optional whitespace (so `adapters=e1, e2` parses as `[e1, e2]`).
fn notifications(tail: &str, client: &RpcClient, timeout: Duration) -> Result<MetaOutcome> {
    let mut parts = tail.split_whitespace();
    let head = parts.next().unwrap_or("");
    let enabled = match head {
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => {
            return Err(Error::Rpc(format!(
                ":notifications expects on|off [adapters=…] [sessions=…], got `{other}`"
            )));
        }
    };
    let mut adapters: Vec<String> = Vec::new();
    let mut sessions: Vec<String> = Vec::new();
    let mut current: Option<&mut Vec<String>> = None;
    for part in parts {
        if let Some(rest) = part.strip_prefix("adapters=") {
            adapters.extend(parse_id_list(rest));
            current = Some(&mut adapters);
        } else if let Some(rest) = part.strip_prefix("sessions=") {
            sessions.extend(parse_id_list(rest));
            current = Some(&mut sessions);
        } else if let Some(target) = current.as_deref_mut() {
            target.extend(parse_id_list(part));
        } else {
            return Err(Error::Rpc(format!(
                ":notifications: unknown filter token `{part}` (expected `adapters=…` or `sessions=…`)"
            )));
        }
    }
    if !enabled && (!adapters.is_empty() || !sessions.is_empty()) {
        return Err(Error::Rpc(
            ":notifications off does not take filter args".to_string(),
        ));
    }
    let mut params = Map::new();
    params.insert("enabled".to_string(), Value::Bool(enabled));
    if !adapters.is_empty() {
        params.insert(
            "adapters".to_string(),
            Value::Array(adapters.into_iter().map(Value::String).collect()),
        );
    }
    if !sessions.is_empty() {
        params.insert(
            "sessions".to_string(),
            Value::Array(sessions.into_iter().map(Value::String).collect()),
        );
    }
    let result = client.call("server.set_notifications", Value::Object(params), timeout)?;
    // Pause / resume the heartbeat in lockstep with the subscription
    // gate — otherwise the heartbeat would re-assert `enabled = true`
    // every interval and silently undo `:notifications off`.
    client.set_heartbeat_enabled(enabled);
    Ok(MetaOutcome::Json(result))
}

fn parse_id_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn rpc_passthrough(tail: &str, client: &RpcClient, timeout: Duration) -> Result<MetaOutcome> {
    let mut split = tail.splitn(2, char::is_whitespace);
    let method = split
        .next()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| Error::Rpc(":rpc requires a method name".to_string()))?
        .to_string();
    let params_text = split.next().unwrap_or("{}").trim();
    let params: Value = if params_text.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(params_text)
            .map_err(|error| Error::Rpc(format!(":rpc params are not valid JSON: {error}")))?
    };
    let result = client.call(&method, params, timeout)?;
    Ok(MetaOutcome::Json(result))
}

/// Inline help text printed by `:help`. The wording here matches the
/// post-Lua-rewrite REPL — kwargs are shown as trailing tables, regex
/// literals use bare strings (or the `re("…")` helper), durations use
/// `ms(N)` / `s(N)`.
pub fn help_text() -> &'static str {
    "ptywright repl commands (real Lua 5.4):\n\
     \n\
     Each line is evaluated as Lua. The globals below are bound by the REPL;\n\
     the full standard library is also available (`os`, `io`, `string`, …).\n\
     The `{}` form is Lua's call-with-table-argument sugar — preferred for\n\
     kwargs-heavy calls because it reads cleaner than the parenthesised form.\n\
     \n\
     Sessions:\n\
       plugins()                              list built-in plugins\n\
       plugins.describe \"name\"                describe a plugin (Lua string sugar)\n\
       session.spawn{ \"name\", rows = 24 }     spawn an adapter (combined form)\n\
       session.spawn(\"name\", { rows = 24 })   ↳ equivalent parenthesised form\n\
       session.resume{ \"name\", prior_adapter = \"id\" }\n\
       session.list()                         known adapters (this REPL)\n\
       session.live()                         adapters live on the server\n\
       session.attach \"id\"   /   session.attach \"all\"\n\
       session.close()                        close the focused adapter\n\
       state()                                re-classify the focused adapter\n\
     \n\
     Driving the focused adapter:\n\
       send.text \"hello\"                      send a prompt (intent=send_prompt)\n\
       send.key \"enter\"                       send a single key\n\
       send.intent{ \"name\", k = v, … }        invoke a plugin intent\n\
       turn{ \"send_prompt\", prompt = \"go\", wait = matches \"done\", timeout = s(5) }\n\
       wait(matches \"^❯\")                     wait for the screen to match a regex\n\
       wait(screen_stable(ms(250)))           wait for the screen to settle\n\
       wait.matches \"^❯\"                      shorthand: wait with the default matcher\n\
       wait.screen_stable(250)                shorthand: wait for stability (ms)\n\
       cancel_wait \"wait-id\"                  break a still-in-flight wait\n\
       transcript.snapshot{ redact = false }  dump the transcript (default redact=true)\n\
       screen.snapshot() / view()             render the focused PTY inline (styled)\n\
       inspect()                              diagnostic dump (adapter.inspect)\n\
     \n\
     Helpers:\n\
       re \"^❯\"      identity — documents \"this string is a regex\"\n\
       ms(250)      identity — documents \"this number is milliseconds\"\n\
       s(2)         shorthand for 2000 ms (seconds → ms)\n\
     \n\
     Meta:\n\
       :tabs       :focus <id>   :live   :attach <id|all>\n\
       :notifications on|off [adapters=…] [sessions=…]\n\
       :rpc <method> {json}\n\
       :tips       guided tour of the most useful idioms\n\
       :quit       :help\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::ctx::AdapterTab;

    #[test]
    fn format_tabs_marks_focused_adapter() {
        let mut ctx = ReplCtx::new();
        ctx.adapters.push(AdapterTab {
            id: "e1".into(),
            plugin: "claude-code".into(),
            state_label: None,
        });
        ctx.adapters.push(AdapterTab {
            id: "e2".into(),
            plugin: "claude-code".into(),
            state_label: None,
        });
        ctx.focus = Some("e2".into());
        let line = format_tabs(&ctx);
        assert!(line.contains("e1:claude-code"));
        assert!(line.contains("*e2:claude-code"));
    }

    #[test]
    fn format_tabs_says_no_adapters_when_empty() {
        let ctx = ReplCtx::new();
        assert_eq!(format_tabs(&ctx), "(no adapters)");
    }

    #[test]
    fn parse_id_list_strips_whitespace_and_empty_tokens() {
        assert_eq!(parse_id_list("e1, e2,e3"), vec!["e1", "e2", "e3"]);
        assert_eq!(parse_id_list("e1"), vec!["e1"]);
        assert_eq!(parse_id_list(""), Vec::<String>::new());
        assert_eq!(parse_id_list(", ,"), Vec::<String>::new());
    }

    #[test]
    fn tips_dispatch_returns_show_tips_with_guide_body() {
        // `:tips` shouldn't hit the RPC server — drive it with a dummy
        // pipe so a stray network call would deadlock the test and we'd
        // notice in CI.
        let (c2s_r, c2s_w) = std::io::pipe().expect("pipe c→s");
        let (s2c_r, _s2c_w) = std::io::pipe().expect("pipe s→c");
        // The reader half is held to keep the writer happy; we just
        // need a client that can sit idle.
        drop(c2s_r);
        let client =
            crate::repl::transport::RpcClient::new(s2c_r, c2s_w, crate::repl::Framing::Ndjson);
        let mut ctx = ReplCtx::new();
        let outcome = dispatch("tips", &client, &mut ctx, Duration::from_millis(50))
            .expect(":tips should dispatch without error");
        match outcome {
            MetaOutcome::ShowTips(body) => {
                assert!(body.contains("plugins()"), "guide body missing core idiom");
                assert!(body.contains(":help"));
            }
            other => panic!("expected ShowTips, got {other:?}"),
        }
    }

    /// An `RpcClient` wired to dead pipes. Every meta command tested
    /// below resolves before issuing an RPC call, so the client only
    /// needs to exist — a real network round-trip would deadlock and
    /// surface as a test hang.
    fn idle_client() -> std::sync::Arc<crate::repl::transport::RpcClient> {
        let (c2s_r, c2s_w) = std::io::pipe().expect("pipe c→s");
        let (s2c_r, _s2c_w) = std::io::pipe().expect("pipe s→c");
        drop(c2s_r);
        crate::repl::transport::RpcClient::new(s2c_r, c2s_w, crate::repl::Framing::Ndjson)
    }

    #[test]
    fn help_quit_and_tabs_dispatch_locally() {
        let client = idle_client();
        let mut ctx = ReplCtx::new();
        let t = Duration::from_millis(50);

        assert!(matches!(
            dispatch("help", &client, &mut ctx, t).unwrap(),
            MetaOutcome::ShowHelp(_)
        ));
        for quit in ["quit", "q", "exit"] {
            assert!(matches!(
                dispatch(quit, &client, &mut ctx, t).unwrap(),
                MetaOutcome::Quit
            ));
        }
        let MetaOutcome::Line(text) = dispatch("tabs", &client, &mut ctx, t).unwrap() else {
            panic!("expected Line from :tabs");
        };
        assert_eq!(text, "(no adapters)");
    }

    #[test]
    fn focus_switches_and_rejects_unknown_or_missing_ids() {
        let client = idle_client();
        let mut ctx = ReplCtx::new();
        ctx.upsert_adapter("e1", "claude-code");
        let t = Duration::from_millis(50);

        let MetaOutcome::Line(text) = dispatch("focus e1", &client, &mut ctx, t).unwrap() else {
            panic!("expected Line from :focus");
        };
        assert!(text.contains("e1"));
        assert_eq!(ctx.focus.as_deref(), Some("e1"));

        // Missing arg and unknown id both error before any RPC call.
        assert!(dispatch("focus", &client, &mut ctx, t).is_err());
        assert!(dispatch("focus nope", &client, &mut ctx, t).is_err());
    }

    #[test]
    fn meta_arg_validation_errors_before_any_rpc_call() {
        let client = idle_client();
        let mut ctx = ReplCtx::new();
        let t = Duration::from_millis(50);

        // Unknown meta command.
        let err = dispatch("bogus", &client, &mut ctx, t).unwrap_err();
        assert!(err.to_string().contains("bogus"));
        // `:attach` with no spec.
        assert!(dispatch("attach", &client, &mut ctx, t).is_err());
        // `:notifications` with an unparseable on/off head.
        assert!(dispatch("notifications maybe", &client, &mut ctx, t).is_err());
        // `:notifications off` does not accept filter args.
        assert!(dispatch("notifications off adapters=e1", &client, &mut ctx, t).is_err());
        // `:notifications on` with an unknown filter token.
        assert!(dispatch("notifications on bogus=e1", &client, &mut ctx, t).is_err());
        // `:rpc` with no method name.
        assert!(dispatch("rpc", &client, &mut ctx, t).is_err());
        // `:rpc` with malformed JSON params.
        assert!(dispatch("rpc server.capabilities {bad", &client, &mut ctx, t).is_err());
    }
}
