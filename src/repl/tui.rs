//! Sequential reedline-based REPL.
//!
//! Each command is rendered as `pty> <syntax-highlighted DSL>` and the
//! result follows on the next line as `↳ <dim summary>`, matching the
//! original mockup. Line editing, completion, syntax highlighting,
//! history, and ghost-text hinting are all delegated to reedline; this
//! module only handles the read-eval-print loop, the prompt, and how
//! each `CmdOutcome` is printed.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode,
    PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, Signal, default_emacs_keybindings,
};
use serde_json::Value;

use super::command::{CmdOutcome, parse};
use super::completer::{PluginCache, ReplCompleter};
use super::ctx::ReplCtx;
use super::highlighter::ReplHighlighter;
use super::transport::RpcClient;
use crate::error::{Error, Result};
use crate::paths::Paths;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Public entry. Builds reedline + prompt and runs the read-eval-print
/// loop until the user quits with `:quit` / `Ctrl-D`.
pub fn run(client: Arc<RpcClient>, transport_label: String) -> Result<()> {
    let ctx = Arc::new(Mutex::new(ReplCtx::new()));
    let plugins = PluginCache::new();

    // Seed the plugin cache so the very first Tab inside
    // `session.spawn("…")` knows which plugin names exist.
    if let Ok(value) = client.call("adapter.list", serde_json::json!({}), RPC_TIMEOUT)
        && let Some(names) = extract_plugin_names(&value)
    {
        plugins.set(names);
    }

    // Persistent history at `~/.ptywright/repl-history` (or wherever
    // `PTYWRIGHT_HOME` points).
    let history_path = Paths::from_env().repl_history_path();
    let history = super::history::open(&history_path)?;

    let completer = Box::new(ReplCompleter::new(Arc::clone(&ctx), plugins));
    let highlighter = Box::new(ReplHighlighter::new());
    let hinter =
        Box::new(DefaultHinter::default().with_style(Style::new().italic().fg(Color::DarkGray)));

    // Register a columnar completion menu so Tab shows candidates and
    // Tab / Shift-Tab cycle through them.
    let menu = ReedlineMenu::EngineCompleter(Box::new(
        ColumnarMenu::default().with_name("completion_menu"),
    ));
    let mut keybinds = default_emacs_keybindings();
    keybinds.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybinds.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    let edit_mode = Box::new(Emacs::new(keybinds));

    let mut line_editor = Reedline::create()
        .with_completer(completer)
        .with_highlighter(highlighter)
        .with_history(Box::new(history))
        .with_hinter(hinter)
        .with_menu(menu)
        .with_edit_mode(edit_mode);

    let prompt = PtywrightPrompt {
        transport_label: transport_label.clone(),
    };

    // Banner. `nu-ansi-term` prints inline ANSI; the terminal is in
    // line-by-line mode so the codes do not interfere with reedline's
    // column accounting.
    println!(
        "{}  {}",
        Color::Cyan.bold().paint("ptywright repl"),
        Style::new().dimmed().paint(&transport_label),
    );
    println!(
        "{}",
        Style::new()
            .dimmed()
            .paint("Type :help for commands · :quit to exit · Tab/Shift-Tab cycles completions",),
    );
    println!();

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let cmd = match parse(trimmed) {
                    Ok(cmd) => cmd,
                    Err(error) => {
                        print_error(&error.to_string());
                        continue;
                    }
                };
                let outcome = {
                    let mut ctx = ctx.lock().expect("repl ctx mutex");
                    super::command::dispatch(cmd, &client, &mut ctx, RPC_TIMEOUT)
                };
                match outcome {
                    Ok(CmdOutcome::Quit) => return Ok(()),
                    Ok(CmdOutcome::ShowHelp(text)) => print_help(&text),
                    Ok(CmdOutcome::Line(text)) => print_line_result(&text),
                    Ok(CmdOutcome::Json(value)) => print_json_result(&value),
                    Err(error) => print_error(&error.to_string()),
                }
            }
            Ok(Signal::CtrlC) => {
                println!("{}", Style::new().dimmed().paint("(input cancelled)"));
                continue;
            }
            Ok(Signal::CtrlD) => {
                println!("{}", Style::new().dimmed().paint("bye."));
                return Ok(());
            }
            // `Signal` is non-exhaustive — future reedline versions may
            // add new variants. Treat anything else as "keep going" so
            // adding a Signal doesn't break a release-build REPL.
            Ok(_) => continue,
            Err(error) => {
                return Err(Error::Rpc(format!("reedline error: {error}")));
            }
        }
    }
}

fn extract_plugin_names(value: &Value) -> Option<Vec<String>> {
    let plugins = value.get("plugins")?.as_array()?;
    Some(
        plugins
            .iter()
            .filter_map(|p| p.get("name").and_then(Value::as_str).map(str::to_string))
            .collect(),
    )
}

// ---- result printing ---------------------------------------------------

fn print_line_result(text: &str) {
    let mut first = true;
    for line in text.split('\n') {
        if first {
            println!("  {}", Style::new().dimmed().paint(format!("↳ {line}")));
            first = false;
        } else if !line.is_empty() {
            println!("    {}", Style::new().dimmed().paint(line));
        }
    }
}

fn print_json_result(value: &Value) {
    let text = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    let max = 240;
    let truncated = if text.chars().count() > max {
        format!("{}…", text.chars().take(max).collect::<String>())
    } else {
        text
    };
    println!(
        "  {} {}",
        Color::Cyan.paint("↳"),
        Style::new().dimmed().paint(truncated),
    );
}

fn print_help(text: &str) {
    println!();
    println!("{}", Color::Yellow.bold().paint("ptywright repl · help"));
    for line in text.lines() {
        // Lightly color section headers (lines ending in `:`) and indent
        // command rows so help is scannable even though we stream it as
        // plain stdout.
        if line.ends_with(':') {
            println!("{}", Color::Yellow.paint(line));
        } else if line.is_empty() {
            println!();
        } else {
            println!("{}", Style::new().dimmed().paint(line));
        }
    }
    println!();
}

fn print_error(message: &str) {
    println!(
        "  {} {}",
        Color::Red.bold().paint("✗"),
        Color::Red.paint(message),
    );
}

// ---- Prompt impl --------------------------------------------------------

struct PtywrightPrompt {
    transport_label: String,
}

impl Prompt for PtywrightPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Owned(format!("{}", Color::Green.bold().paint("pty> ")))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(format!(
            "{}",
            Style::new().dimmed().paint(self.transport_label.clone()),
        ))
    }

    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("  … ")
    }

    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("(history) ")
    }
}
