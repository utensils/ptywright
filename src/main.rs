use std::borrow::Cow;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use ptywright::{DESCRIPTION, NAME, Target, TerminalSize, serve_lsp, serve_ndjson};

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

fn run_command(mut command: Vec<String>, size: TerminalSize) -> ptywright::Result<ExitCode> {
    let program = command.remove(0);
    let target = Target::new(program).args(command).size(size);
    let interactive_terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
    let _raw_mode = RawModeGuard::enable_if(interactive_terminal)?;

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: target.size.rows,
        cols: target.size.cols,
        pixel_width: target.size.pixel_width,
        pixel_height: target.size.pixel_height,
    })?;

    let mut builder = CommandBuilder::new(&target.program);
    builder.args(target.args.iter().map(String::as_str));
    let mut child = pair.slave.spawn_command(builder)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    let output_thread = thread::spawn(move || -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    stdout.write_all(&buf[..n])?;
                    stdout.flush()?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    });

    let _input_thread = thread::spawn(move || -> io::Result<()> {
        let mut stdin = io::stdin().lock();
        let mut buf = [0_u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let input = if interactive_terminal {
                        filter_terminal_generated_input(&buf[..n])
                    } else {
                        Cow::Borrowed(&buf[..n])
                    };
                    if !input.is_empty() {
                        writer.write_all(&input)?;
                        writer.flush()?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    });

    let status = child.wait()?;
    drop(pair.master);
    if let Ok(Err(error)) = output_thread.join() {
        return Err(error.into());
    }

    if status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(status.exit_code().min(u8::MAX as u32) as u8))
    }
}

struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    fn enable_if(enabled: bool) -> io::Result<Self> {
        if enabled {
            crossterm::terminal::enable_raw_mode()?;
        }
        Ok(Self { enabled })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn filter_terminal_generated_input(input: &[u8]) -> Cow<'_, [u8]> {
    let mut index = 0;
    let mut output = Vec::new();
    let mut changed = false;

    while index < input.len() {
        if let Some(len) = terminal_generated_sequence_len(&input[index..]) {
            changed = true;
            index += len;
        } else {
            output.push(input[index]);
            index += 1;
        }
    }

    if changed {
        Cow::Owned(output)
    } else {
        Cow::Borrowed(input)
    }
}

fn terminal_generated_sequence_len(input: &[u8]) -> Option<usize> {
    match input {
        [0x1b, b'[', b'I', ..] | [0x1b, b'[', b'O', ..] => Some(3),
        [0x1b, b'[', b'?', rest @ ..] => csi_final_len(rest).and_then(|len| {
            if rest.get(len - 1) == Some(&b'c') {
                Some(3 + len)
            } else {
                None
            }
        }),
        [0x1b, b'P', rest @ ..] => dcs_final_len(rest).map(|len| 2 + len),
        [0x9b, b'I', ..] | [0x9b, b'O', ..] => Some(2),
        [0x9b, b'?', rest @ ..] => csi_final_len(rest).and_then(|len| {
            if rest.get(len - 1) == Some(&b'c') {
                Some(2 + len)
            } else {
                None
            }
        }),
        [0x90, rest @ ..] => dcs_final_len(rest).map(|len| 1 + len),
        _ => None,
    }
}

fn csi_final_len(input: &[u8]) -> Option<usize> {
    input
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|index| index + 1)
}

fn dcs_final_len(input: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            0x07 | 0x9c => return Some(index + 1),
            0x1b if input.get(index + 1) == Some(&b'\\') => return Some(index + 2),
            _ => index += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_terminal_focus_events() {
        assert_eq!(
            filter_terminal_generated_input(b"a\x1b[Ib\x1b[Oc").as_ref(),
            b"abc"
        );
    }

    #[test]
    fn filters_terminal_capability_responses() {
        let input = b"hello\x1bP>|ghostty 1.3.1\x1b\\\x1b[?62;22;52cworld";

        assert_eq!(
            filter_terminal_generated_input(input).as_ref(),
            b"helloworld"
        );
    }

    #[test]
    fn preserves_user_navigation_sequences() {
        let input = b"\x1b[A\x1b[B\x1b[200~paste\x1b[201~";

        assert_eq!(filter_terminal_generated_input(input).as_ref(), input);
    }
}
