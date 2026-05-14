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

## Development checks

Inside the Nix devshell:

```bash
ci-local
```

Equivalent direct commands:

```bash
cargo fmt --all -- --check
cargo check
cargo clippy -- -D warnings
cargo test
cargo build --release
```

## Documentation

```bash
docs-dev      # local VitePress dev server
docs-build    # static docs build
docs-preview  # preview built docs
```
