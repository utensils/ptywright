use std::io::{self, Write};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use ptywright::{DESCRIPTION, NAME, Session, Target, TerminalSize, serve_ndjson};

#[derive(Debug, Parser)]
#[command(
    name = NAME,
    version,
    about = DESCRIPTION,
    long_about = "ptywright is a cross-platform Rust CLI and library for driving interactive terminal applications through PTYs.\n\nThe library now exposes early PTY session, screen snapshot, action, matcher, transcript, and JSON-RPC primitives. App-specific automation, including interactive Claude Code support, will be layered on top of those generic pieces."
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
}

fn main() -> ExitCode {
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
        None => {
            let mut command = Cli::command();
            command.print_help()?;
            println!();
            Ok(ExitCode::SUCCESS)
        }
    }
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
