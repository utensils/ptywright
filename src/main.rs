use std::io::{self, Write};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use ptywright::{DESCRIPTION, NAME, Session, Target, TerminalSize, serve_ndjson};

#[derive(Debug, Parser)]
#[command(
    name = NAME,
    version,
    about = DESCRIPTION,
    long_about = "ptywright is a cross-platform Rust CLI and library for driving interactive terminal applications through PTYs.\n\nThe library exposes early PTY session, screen snapshot, action, matcher, transcript, JSON-RPC, Claude Code adapter, plugin manifest, and shell completion primitives."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run a command in a headless PTY and print its captured transcript when it exits.
    Run {
        /// Terminal rows.
        #[arg(long, default_value_t = 24)]
        rows: u16,
        /// Terminal columns.
        #[arg(long, default_value_t = 80)]
        cols: u16,
        /// Command and arguments to run after `--`.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Serve JSON-RPC 2.0 over stdio using newline-delimited JSON framing.
    Serve {
        /// Use stdin/stdout for JSON-RPC. Stdout is protocol-only in this mode.
        #[arg(long)]
        stdio: bool,
    },
    /// Generate shell completions.
    #[command(after_long_help = "\
Setup instructions:

  zsh (add to ~/.zshrc):
    source <(ptywright completions zsh)

  bash (add to ~/.bashrc):
    source <(ptywright completions bash)

  fish (persist to completions dir):
    ptywright completions fish | source
    ptywright completions fish > ~/.config/fish/completions/ptywright.fish

  elvish:
    eval (ptywright completions elvish | slurp)

  powershell (add to $PROFILE):
    ptywright completions powershell | Out-String | Invoke-Expression")]
    Completions {
        /// Shell to generate completions for (bash, zsh, fish, elvish, powershell).
        shell: String,
    },
}

fn main() -> ExitCode {
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ptywright: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> ptywright::Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Run {
            rows,
            cols,
            command,
        }) => run_command(command, TerminalSize::new(rows, cols)),
        Some(Commands::Serve { stdio }) => serve_command(stdio),
        Some(Commands::Completions { shell }) => generate_completions(&shell),
        None => {
            let mut command = Cli::command();
            command.print_help()?;
            println!();
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn generate_completions(shell: &str) -> ptywright::Result<ExitCode> {
    if shell == "zsh" {
        let bin = std::env::args()
            .next()
            .unwrap_or_else(|| "ptywright".to_string());
        write!(
            io::stdout(),
            r##"#compdef ptywright
function _clap_dynamic_completer_ptywright() {{
    local _CLAP_COMPLETE_INDEX=$(expr $CURRENT - 1)
    local _CLAP_IFS=$'\n'

    local completions=("${{(@f)$( \
        _CLAP_IFS="$_CLAP_IFS" \
        _CLAP_COMPLETE_INDEX="$_CLAP_COMPLETE_INDEX" \
        COMPLETE="zsh" \
        {bin} -- "${{words[@]}}" 2>/dev/null \
    )}}")

    if [[ -n $completions ]]; then
        local -a flags=()
        local -a values=()
        local completion
        for completion in $completions; do
            local value="${{completion%%:*}}"
            if [[ "$value" == -* ]]; then
                flags+=("$completion")
            else
                values+=("$completion")
            fi
        done

        if [[ "${{words[$CURRENT]}}" == -* ]]; then
            [[ -n $flags ]] && _describe 'options' flags
        else
            [[ -n $values ]] && _describe 'values' values
        fi
    fi
}}

compdef _clap_dynamic_completer_ptywright ptywright
"##,
            bin = bin,
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    let shells = clap_complete::env::Shells::builtins();
    let completer = shells.completer(shell).ok_or_else(|| {
        let names = shells.names().collect::<Vec<_>>().join(", ");
        ptywright::Error::Rpc(format!("unknown shell '{shell}', expected one of: {names}"))
    })?;
    let bin = std::env::args()
        .next()
        .unwrap_or_else(|| "ptywright".to_string());
    completer.write_registration(
        "COMPLETE",
        "ptywright",
        "ptywright",
        &bin,
        &mut io::stdout(),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn serve_command(stdio: bool) -> ptywright::Result<ExitCode> {
    if !stdio {
        return Err(ptywright::Error::Rpc(
            "serve currently requires --stdio".to_string(),
        ));
    }
    serve_ndjson(io::stdin().lock(), io::stdout().lock())?;
    Ok(ExitCode::SUCCESS)
}

fn run_command(mut command: Vec<String>, size: TerminalSize) -> ptywright::Result<ExitCode> {
    let program = command.remove(0);
    let target = Target::new(program).args(command).size(size);
    let session = Session::spawn_target(target)?;
    let status = session.wait()?;
    io::stdout().write_all(session.transcript().as_bytes())?;
    if status.success {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(status.code.min(u8::MAX as u32) as u8))
    }
}
