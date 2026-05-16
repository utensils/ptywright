# Quickstart

## From a local checkout

```bash
git clone https://github.com/utensils/ptywright
cd ptywright
nix develop
cargo run -- --help
```

## Basic commands

```bash
ptywright --help
ptywright --version
```

With no arguments, `ptywright` prints the help menu.

## Run a command in a headless PTY

```bash
ptywright run -- /bin/sh -lc 'printf ready'
```

`run` starts the command behind a real PTY and bridges stdin/stdout live. It is a debugging surface for the same PTY/session primitives exposed by the library; use JSON-RPC for machine-readable automation.

## Query JSON-RPC capabilities

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | \
  ptywright serve --stdio
```

`serve --stdio` uses stdin/stdout for JSON-RPC. NDJSON is the default framing; `--framing lsp` enables LSP-style `Content-Length` frames. stdout is protocol-only in this mode. `serve --socket /tmp/ptywright.sock` exposes the same protocol over a local Unix domain socket on macOS/Linux, or over a named pipe (`\\.\pipe\ptywright`-style path) on Windows — both share the same flag.

## Runtime directory

ptywright keeps configuration and rotated log files under `~/.ptywright/`. Override the root with `PTYWRIGHT_HOME=/some/path`, and tune the log filter at runtime with `PTYWRIGHT_LOG`:

```bash
PTYWRIGHT_LOG="info,ptywright::rpc=debug" ptywright serve --stdio
```

See the [Runtime directory guide](./runtime-directory.md) for the full layout, config schema, log rotation/retention rules, and per-mode sink behavior.

## Enable shell completions

```bash
# zsh
source <(ptywright completions zsh)

# bash
source <(ptywright completions bash)

# fish
ptywright completions fish > ~/.config/fish/completions/ptywright.fish
```

Supported shells are bash, zsh, fish, elvish, and PowerShell.

## Development checks

Inside the Nix devshell:

```bash
ci-local
```

Equivalent direct commands:

```bash
cargo fmt --all -- --check
cargo check --locked
cargo clippy --locked -- -D warnings
cargo test --locked --features _test-fixtures
cargo build --release --locked
```

The `_test-fixtures` Cargo feature gates the test-only `ptywright-echo-tui` fixture binary so `cargo install ptywright` never deposits it into your `~/.cargo/bin` alongside the real CLI. CI test runs (and the devshell `run-tests` / `ci-local` commands) enable it explicitly; bare `cargo install` does not.

## Documentation

```bash
docs-dev      # local VitePress dev server
docs-build    # static docs build
docs-preview  # preview built docs
```

Or without the devshell:

```bash
cd website
bun install
bun run build
```
