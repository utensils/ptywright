use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod run_terminal;
use ptywright::{DESCRIPTION, NAME, TerminalSize, serve_lsp, serve_ndjson};

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
    /// Run a command in a headless PTY, bridging stdin/stdout live.
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
    /// Serve JSON-RPC 2.0 over stdio.
    Serve {
        /// Use stdin/stdout for JSON-RPC. Stdout is protocol-only in this mode.
        #[arg(long)]
        stdio: bool,
        /// Listen on a local Unix socket path. Unix-only; use --stdio on Windows for now.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// JSON-RPC message framing to use.
        #[arg(long, value_enum, default_value_t = RpcFraming::Ndjson)]
        framing: RpcFraming,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RpcFraming {
    /// Newline-delimited JSON, one JSON-RPC message per line.
    Ndjson,
    /// LSP-style Content-Length headers followed by JSON payloads.
    Lsp,
}

fn main() -> ExitCode {
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}", cli_error_message(&error));
            ExitCode::FAILURE
        }
    }
}

fn cli_error_message(error: &ptywright::Error) -> String {
    let message = ptywright::RedactionPolicy::default().redact(&error.to_string());
    format!("ptywright: {message}")
}

fn run() -> ptywright::Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Run {
            rows,
            cols,
            command,
        }) => run_terminal::run_command(command, TerminalSize::new(rows, cols)),
        Some(Commands::Serve {
            stdio,
            socket,
            framing,
        }) => serve_command(stdio, socket.as_deref(), framing),
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

fn serve_command(
    stdio: bool,
    socket: Option<&Path>,
    framing: RpcFraming,
) -> ptywright::Result<ExitCode> {
    match (stdio, socket) {
        (true, None) => match framing {
            RpcFraming::Ndjson => serve_ndjson(io::stdin().lock(), io::stdout().lock())?,
            RpcFraming::Lsp => serve_lsp(io::stdin().lock(), io::stdout().lock())?,
        },
        (false, Some(path)) => serve_socket(path, framing)?,
        (true, Some(_)) => {
            return Err(ptywright::Error::Rpc(
                "serve accepts only one transport: use either --stdio or --socket".to_string(),
            ));
        }
        (false, None) => {
            return Err(ptywright::Error::Rpc(
                "serve requires --stdio or --socket".to_string(),
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(unix)]
fn serve_socket(path: &Path, framing: RpcFraming) -> ptywright::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixListener;

    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.file_type().is_socket() {
            std::fs::remove_file(path)?;
        } else {
            return Err(ptywright::Error::Rpc(format!(
                "refusing to replace non-socket path: {}",
                path.display()
            )));
        }
    }

    let listener = UnixListener::bind(path)?;
    eprintln!("ptywright: listening on {}", path.display());
    for stream in listener.incoming() {
        let stream = stream?;
        let input = stream.try_clone()?;
        match framing {
            RpcFraming::Ndjson => serve_ndjson(input, stream)?,
            RpcFraming::Lsp => serve_lsp(input, stream)?,
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn serve_socket(path: &Path, _framing: RpcFraming) -> ptywright::Result<()> {
    Err(ptywright::Error::Rpc(format!(
        "--socket is not supported on this platform yet; Windows named-pipe support is planned (requested {})",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_error_messages_are_redacted() {
        let error = ptywright::Error::Rpc("token=super-secret-value".to_string());

        let message = cli_error_message(&error);

        assert!(message.contains("token=[REDACTED]"));
        assert!(!message.contains("super-secret-value"));
    }
}
