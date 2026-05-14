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

`run` starts the command behind a real PTY, waits for it to exit, and prints the retained transcript. It is an early debugging surface for the same PTY/session primitives exposed by the library; live interactive bridging is planned.

## Query JSON-RPC capabilities

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"server.capabilities"}\n' | \
  ptywright serve --stdio
```

`serve --stdio` uses stdin/stdout for NDJSON-framed JSON-RPC. stdout is protocol-only in this mode.

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
cargo test --locked
cargo build --release --locked
```

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
