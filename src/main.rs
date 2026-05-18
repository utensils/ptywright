use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod run_terminal;
use ptywright::{
    Config, DESCRIPTION, LogGuard, LoggingConfig, NAME, Paths, RpcServerState, TerminalSize,
    init_for_oneshot, init_for_run, init_for_serve_socket, init_for_serve_stdio,
    serve_lsp_with_state, serve_ndjson_with_state,
};

#[derive(Debug, Parser)]
#[command(
    name = NAME,
    version,
    about = DESCRIPTION,
    long_about = "ptywright is a cross-platform Rust CLI and library for driving interactive terminal applications through PTYs.\n\nThe library exposes early PTY session, screen snapshot, action, matcher, transcript, JSON-RPC, generic plugin-backed extension, plugin manifest, and shell completion primitives. Application-specific TUI behaviour lives in trusted Lua plugins under plugins/<name>/."
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
        /// Listen on a local IPC path: Unix domain socket on macOS/Linux, named pipe on Windows
        /// (e.g. \\.\pipe\ptywright). Use --stdio for single-client stdin/stdout transport.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// JSON-RPC message framing to use.
        #[arg(long, value_enum, default_value_t = RpcFraming::Ndjson)]
        framing: RpcFraming,
        /// Pre-load a trusted-local third-party plugin from a TOML manifest.
        /// May be repeated to load multiple plugins. The manifest's
        /// `entrypoint` is resolved relative to the manifest file's parent
        /// directory. Plugins loaded this way are visible through
        /// `adapter.list` and instantiable via `adapter.start`. Only load
        /// plugins from trusted local sources — they execute in the same
        /// trust domain as the built-in claude-code plugin.
        #[arg(long = "plugin", value_name = "MANIFEST.TOML", action = clap::ArgAction::Append)]
        plugin: Vec<PathBuf>,
        /// Enable the `plugin.load` / `plugin.unload` JSON-RPC methods so
        /// connected clients can register additional trusted-local plugins
        /// at runtime. Off by default; combine with `--plugin` for
        /// boot-time registration without runtime mutation.
        #[arg(long)]
        allow_plugin_load: bool,
    },
    /// Interactive REPL client for a running `ptywright serve`.
    ///
    /// Requires building with `--features repl`. With the feature off, the
    /// subcommand is omitted from `--help` and parsing rejects it as
    /// unknown.
    #[cfg(feature = "repl")]
    Repl {
        /// Connect to a long-running `ptywright serve --socket <path>`.
        #[arg(long, conflicts_with = "stdio", group = "transport")]
        socket: Option<PathBuf>,
        /// Spawn a child server and speak JSON-RPC over its stdio. Pass
        /// the child command after `--`.
        #[arg(long, group = "transport")]
        stdio: bool,
        /// JSON-RPC framing for the connection.
        #[arg(long, value_enum, default_value_t = RpcFraming::Ndjson)]
        framing: RpcFraming,
        /// Child command and args. Required iff --stdio. Pass after `--`,
        /// e.g. `ptywright repl --stdio -- ptywright serve --stdio`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            requires = "stdio"
        )]
        command: Vec<String>,
    },
    /// Tail the most recent ptywright log file under `~/.ptywright/logs/`.
    ///
    /// Reads `ptywright.YYYY-MM-DD.log` (the newest file matching the
    /// rotation pattern) and prints the last `--lines` lines, then
    /// follows the file for new content until Ctrl-C. Use `--filter`
    /// to restrict output to lines containing a substring. Honours
    /// `PTYWRIGHT_HOME` for the runtime directory root.
    Logs {
        /// Filter to log lines containing this substring.
        #[arg(long, value_name = "SUBSTRING")]
        filter: Option<String>,
        /// Print the last N lines before following the file. Defaults
        /// to 20 to mirror `tail -n 20 -f` behaviour.
        #[arg(long, default_value_t = 20)]
        lines: u64,
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
    let paths = Paths::from_env();
    // Capture any config-load failure so it can be reported through the
    // logging stack (with redaction + per-mode sinks) instead of an
    // unredacted `eprintln!` that would corrupt the live PTY in `run` mode.
    let (config, config_load_error) = match Config::load_or_default(&paths.config_path()) {
        Ok(config) => (config, None),
        Err(error) => (Config::default(), Some(error)),
    };
    // Hold the guard for the lifetime of this function; dropped at exit so
    // tracing-appender flushes its non-blocking buffers.
    let _log_guard = init_logging_for(cli.command.as_ref(), &paths, &config.logging);
    if let Some(error) = config_load_error {
        tracing::warn!(
            error = %error,
            "failed to load config; falling back to defaults"
        );
    }

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
            plugin,
            allow_plugin_load,
        }) => serve_command(
            stdio,
            socket.as_deref(),
            framing,
            &plugin,
            allow_plugin_load,
        ),
        #[cfg(feature = "repl")]
        Some(Commands::Repl {
            socket,
            stdio,
            framing,
            command,
        }) => repl_command(socket, stdio, framing, command),
        Some(Commands::Logs { filter, lines }) => logs_command(&paths, filter, lines),
        Some(Commands::Completions { shell }) => generate_completions(&shell),
        None => {
            let mut command = Cli::command();
            command.print_help()?;
            println!();
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn init_logging_for(
    command: Option<&Commands>,
    paths: &Paths,
    logging: &LoggingConfig,
) -> LogGuard {
    match command {
        Some(Commands::Run { .. }) => init_for_run(paths, logging),
        Some(Commands::Serve { stdio, socket, .. }) => {
            if *stdio {
                init_for_serve_stdio(paths, logging)
            } else if socket.is_some() {
                init_for_serve_socket(paths, logging)
            } else {
                // No transport selected — serve_command will return an error
                // shortly. Use minimal logging until then.
                init_for_oneshot(logging)
            }
        }
        #[cfg(feature = "repl")]
        Some(Commands::Repl { .. }) => init_for_oneshot(logging),
        Some(Commands::Completions { .. }) | Some(Commands::Logs { .. }) | None => {
            init_for_oneshot(logging)
        }
    }
}

fn logs_command(paths: &Paths, filter: Option<String>, lines: u64) -> ptywright::Result<ExitCode> {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
    use std::thread::sleep;
    use std::time::Duration;

    let logs_dir = paths.logs_dir();
    let path = newest_log_file(&logs_dir)?;
    let mut file = File::open(&path).map_err(|error| {
        ptywright::Error::Config(format!("open log file `{}`: {error}", path.display()))
    })?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "==> tailing {} <==", path.display()).ok();

    // Stream the last `lines` lines first so the operator sees recent
    // context, then continue streaming any new content. Simpler than
    // a true reverse-scan: read everything, keep the tail, then seek
    // to the current end for follow mode. Daily-rotated logs stay
    // small enough that the full read is cheap.
    let mut buffer = String::new();
    file.read_to_string(&mut buffer).map_err(|error| {
        ptywright::Error::Config(format!("read log file `{}`: {error}", path.display()))
    })?;
    let len = buffer.lines().count();
    let skip = len.saturating_sub(lines as usize);
    for line in buffer.lines().skip(skip) {
        if filter.as_deref().is_none_or(|needle| line.contains(needle)) {
            writeln!(out, "{line}").ok();
        }
    }
    out.flush().ok();

    // Follow mode. Re-open via BufReader for efficient line-at-a-time
    // reads and seek to the position we already consumed.
    let position = file.stream_position().map_err(|error| {
        ptywright::Error::Config(format!("tell on log file `{}`: {error}", path.display()))
    })?;
    let file = File::open(&path).map_err(|error| {
        ptywright::Error::Config(format!("reopen log file `{}`: {error}", path.display()))
    })?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(position)).ok();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => sleep(Duration::from_millis(200)),
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if filter
                    .as_deref()
                    .is_none_or(|needle| trimmed.contains(needle))
                {
                    writeln!(out, "{trimmed}").ok();
                    out.flush().ok();
                }
            }
            Err(error) => {
                eprintln!("ptywright logs: read error: {error}");
                return Ok(ExitCode::FAILURE);
            }
        }
    }
}

/// Find the newest `ptywright.YYYY-MM-DD.log` file under `logs_dir`.
/// Returns an error if the directory is missing or empty.
fn newest_log_file(logs_dir: &Path) -> ptywright::Result<PathBuf> {
    let entries = std::fs::read_dir(logs_dir).map_err(|error| {
        ptywright::Error::Config(format!("read logs dir `{}`: {error}", logs_dir.display()))
    })?;
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_managed_log_filename)
        })
        .collect();
    if candidates.is_empty() {
        return Err(ptywright::Error::Config(format!(
            "no ptywright log files under `{}` — run ptywright at least once first",
            logs_dir.display()
        )));
    }
    candidates.sort();
    Ok(candidates.pop().expect("non-empty after check"))
}

/// Whether `name` matches the daily-rotation pattern that
/// `tracing-appender` writes: `ptywright.YYYY-MM-DD` with an optional
/// `.log` suffix. Restricts the `ptywright logs` glob so unrelated
/// dotfiles in the logs directory (`ptywright.notes`, `ptywright.tmp`,
/// editor swapfiles) can't outrank the real log on lexicographic
/// sort.
fn is_managed_log_filename(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("ptywright.") else {
        return false;
    };
    // Allow either `ptywright.YYYY-MM-DD` or
    // `ptywright.YYYY-MM-DD.log`; the date part itself must look like
    // a date (10 chars matching `dddd-dd-dd`).
    let date_part = rest.strip_suffix(".log").unwrap_or(rest);
    if date_part.len() != 10 {
        return false;
    }
    date_part.chars().enumerate().all(|(i, c)| match i {
        4 | 7 => c == '-',
        _ => c.is_ascii_digit(),
    })
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

#[cfg(feature = "repl")]
fn repl_command(
    socket: Option<PathBuf>,
    stdio: bool,
    framing: RpcFraming,
    command: Vec<String>,
) -> ptywright::Result<ExitCode> {
    use ptywright::repl::{Framing, ReplArgs, Transport};
    let transport = match (socket, stdio) {
        (Some(path), false) => Transport::Socket(path),
        (None, true) => {
            if command.is_empty() {
                return Err(ptywright::Error::Rpc(
                    "ptywright repl --stdio requires a child command after `--`".to_string(),
                ));
            }
            Transport::Stdio(command)
        }
        (Some(_), true) => {
            return Err(ptywright::Error::Rpc(
                "ptywright repl accepts only one transport: --socket or --stdio".to_string(),
            ));
        }
        (None, false) => {
            // No transport flag → connect to the per-user default socket.
            // Matches `ptywright serve` running without --socket below.
            let path = Paths::from_env().default_socket_path();
            // Pre-check for the most common first-run failure (no server
            // running) so the operator gets a friendlier message than
            // ENOENT bubbled up from inside the transport.
            #[cfg(unix)]
            if !path.exists() {
                return Err(ptywright::Error::Rpc(format!(
                    "no ptywright server is listening at {path} (the default socket).\n\
                     \n\
                     Start one in another terminal:\n  \
                     ptywright serve\n\
                     \n\
                     …or pipe a child server through stdio in one command:\n  \
                     ptywright repl --stdio -- ptywright serve --stdio\n\
                     \n\
                     To use a non-default path, pass --socket on both sides:\n  \
                     ptywright serve --socket /tmp/p.sock &\n  \
                     ptywright repl   --socket /tmp/p.sock",
                    path = path.display(),
                )));
            }
            tracing::info!(socket = %path.display(), "ptywright repl: connecting to default socket");
            Transport::Socket(path)
        }
    };
    let framing = match framing {
        RpcFraming::Ndjson => Framing::Ndjson,
        RpcFraming::Lsp => Framing::Lsp,
    };
    ptywright::repl::run(ReplArgs { transport, framing })
}

fn serve_command(
    stdio: bool,
    socket: Option<&Path>,
    framing: RpcFraming,
    plugin_paths: &[PathBuf],
    allow_plugin_load: bool,
) -> ptywright::Result<ExitCode> {
    let state = build_rpc_state(plugin_paths, allow_plugin_load)?;
    match (stdio, socket) {
        (true, None) => match framing {
            RpcFraming::Ndjson => {
                serve_ndjson_with_state(io::stdin().lock(), io::stdout().lock(), state)?
            }
            RpcFraming::Lsp => {
                serve_lsp_with_state(io::stdin().lock(), io::stdout().lock(), state)?
            }
        },
        (false, Some(path)) => serve_socket(path, framing, state)?,
        (true, Some(_)) => {
            return Err(ptywright::Error::Rpc(
                "serve accepts only one transport: use either --stdio or --socket".to_string(),
            ));
        }
        (false, None) => {
            // No transport flag → bind the per-user default socket. Print
            // the chosen path so the operator can wire a REPL up against
            // it (and so they know `--socket` is the override).
            let default = Paths::from_env().default_socket_path();
            if let Some(parent) = default.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            eprintln!(
                "ptywright: serving on {} (override with --socket)",
                default.display()
            );
            tracing::info!(socket = %default.display(), "ptywright: serving on default socket");
            serve_socket(&default, framing, state)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Construct a shared `RpcServerState`, pre-load any third-party plugins the
/// operator passed via `--plugin <manifest.toml>`, and gate `plugin.load` /
/// `plugin.unload` based on `--allow-plugin-load`. CLI plugin paths are
/// trusted by virtue of being passed by the operator at server startup.
fn build_rpc_state(
    plugin_paths: &[PathBuf],
    allow_plugin_load: bool,
) -> ptywright::Result<RpcServerState> {
    let state = RpcServerState::new();
    for path in plugin_paths {
        let (manifest, source) = ptywright::PluginManifest::load_from_toml_path(path)?;
        let name = manifest.name.clone();
        state.register_plugin(manifest, source)?;
        tracing::info!(plugin = %name, manifest = %path.display(), "ptywright: registered plugin");
    }
    state.set_allow_plugin_load(allow_plugin_load);
    if allow_plugin_load {
        tracing::info!("ptywright: plugin.load enabled via --allow-plugin-load");
    }
    Ok(state)
}

/// Backing storage for the socket-cleanup signal handler. Declared at
/// module scope so the C-ABI shim ([`handle_shutdown_signal`]) can reach
/// it — signal handlers cannot capture environment.
///
/// Holds a NUL-terminated `CString` rather than a `PathBuf` so the
/// handler can call the async-signal-safe `unlink(2)` syscall directly
/// instead of going through `std::fs::remove_file`, which allocates and
/// is not on POSIX's signal-safe function list. `OnceLock::get` is
/// lock-free, so reading the path from inside the handler does not need
/// a mutex either — once `install_socket_cleanup` has stored the path,
/// the handler can read it without taking any locks.
#[cfg(unix)]
static SOCKET_CLEANUP_PATH: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();

#[cfg(unix)]
extern "C" fn handle_shutdown_signal(signum: libc::c_int) {
    // SAFETY: every operation below is on POSIX's list of async-signal-
    // safe functions (`unlink`, `_exit`). `OnceLock::get` is lock-free
    // and the `CString` it returns lives at module scope, so reading
    // its pointer is non-allocating. We deliberately do not `_exit`
    // through `std::process::exit` because Drop handlers are not
    // signal-safe; `_exit` returns the conventional `128 + signum`.
    if let Some(c_path) = SOCKET_CLEANUP_PATH.get() {
        unsafe {
            libc::unlink(c_path.as_ptr());
        }
    }
    unsafe { libc::_exit(128 + signum) }
}

/// Install SIGINT/SIGTERM/SIGHUP handlers that unlink the listening
/// socket before the process exits, so a clean Ctrl-C does not leave a
/// dead socket file behind. Server startup already cleans up stale
/// sockets (see [`serve_socket`]), so this is a UX nicety rather than a
/// correctness requirement.
///
/// Idempotent: only the first call records the path and installs the
/// handlers. Subsequent calls (in the unlikely case `serve_socket` is
/// re-entered) silently do nothing rather than racing the handler.
#[cfg(unix)]
fn install_socket_cleanup(path: &Path) {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    // Refuse paths containing an interior NUL — `CString::new` would
    // reject it, and the handler can't unlink such a path anyway.
    let Ok(c_path) = std::ffi::CString::new(bytes) else {
        return;
    };
    // `set` is fallible: a second install attempt with a different path
    // leaves the original recorded path in place. That's intentional —
    // the first server to bind a socket is the one whose path the
    // handler should clean up.
    let _ = SOCKET_CLEANUP_PATH.set(c_path);

    static HANDLERS_INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    HANDLERS_INSTALLED.get_or_init(|| {
        // SAFETY: `libc::signal` itself is async-signal-safe. The
        // handler we install (`handle_shutdown_signal`) calls only
        // `unlink` + `_exit`, both on POSIX's signal-safe list, and
        // reads its path from a lock-free `OnceLock`. Replacing the
        // default SIGINT/SIGTERM/SIGHUP dispositions is intentional —
        // we want clean shutdown.
        let handler = handle_shutdown_signal as *const () as libc::sighandler_t;
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
            libc::signal(libc::SIGHUP, handler);
        }
    });
}

#[cfg(unix)]
fn serve_socket(path: &Path, framing: RpcFraming, state: RpcServerState) -> ptywright::Result<()> {
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
    install_socket_cleanup(path);
    tracing::info!(socket = %path.display(), "ptywright: listening on local socket");
    for stream in listener.incoming() {
        let stream = stream?;
        let input = stream.try_clone()?;
        let state = state.clone();
        std::thread::spawn(move || {
            let result = match framing {
                RpcFraming::Ndjson => serve_ndjson_with_state(input, stream, state),
                RpcFraming::Lsp => serve_lsp_with_state(input, stream, state),
            };
            if let Err(error) = result {
                tracing::warn!(error = %error, "socket client error");
            }
        });
    }
    Ok(())
}

#[cfg(windows)]
fn serve_socket(path: &Path, framing: RpcFraming, state: RpcServerState) -> ptywright::Result<()> {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};

    let name = path.as_os_str().to_fs_name::<GenericFilePath>()?;
    let listener = ListenerOptions::new().name(name).create_sync()?;
    tracing::info!(socket = %path.display(), "ptywright: listening on named pipe");
    for stream in listener.incoming() {
        let stream = stream?;
        let (input, output) = stream.split();
        let state = state.clone();
        std::thread::spawn(move || {
            let result = match framing {
                RpcFraming::Ndjson => serve_ndjson_with_state(input, output, state),
                RpcFraming::Lsp => serve_lsp_with_state(input, output, state),
            };
            if let Err(error) = result {
                tracing::warn!(error = %error, "socket client error");
            }
        });
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn serve_socket(
    path: &Path,
    _framing: RpcFraming,
    _state: RpcServerState,
) -> ptywright::Result<()> {
    Err(ptywright::Error::Rpc(format!(
        "--socket is not supported on this platform yet (requested {})",
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
