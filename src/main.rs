use clap::{CommandFactory, Parser};
use ptywright::{DESCRIPTION, NAME};

#[derive(Debug, Parser)]
#[command(
    name = NAME,
    version,
    about = DESCRIPTION,
    long_about = "ptywright is a Rust CLI and library skeleton for driving interactive terminal applications through PTYs.\n\nToday the binary only exposes help and version output. The library is reserved for general-purpose PTY/TUI automation primitives that can later drive shells, REPLs, full-screen TUIs, and other terminal applications."
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    let mut command = Cli::command();
    command.print_help().expect("failed to render help");
    println!();
}
