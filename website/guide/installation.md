# Installation

ptywright is pre-1.0 — the surface may still shift. Pick whichever channel best matches how you plan to use it.

## From crates.io

```bash
cargo install ptywright
ptywright --help
```

The default build embeds Lua 5.4 for trusted adapter plugins, so `cargo install` needs a working C compiler on the host. The `_test-fixtures` Cargo feature is off by default; `cargo install ptywright` will not deposit the `ptywright-echo-tui` test helper into `~/.cargo/bin`.

To use ptywright as a library, add it to your `Cargo.toml`:

```bash
cargo add ptywright
```

## From source

```bash
git clone https://github.com/utensils/ptywright
cd ptywright
cargo build --release
./target/release/ptywright --help
```

## Nix (macOS and Linux)

```bash
nix run github:utensils/ptywright -- --help
nix profile install github:utensils/ptywright
```

For development:

```bash
nix develop
ci-local
```

## GitHub Releases

The release workflow is shaped to publish these archives:

| Platform            | Archive                                      |
| ------------------- | -------------------------------------------- |
| macOS Apple Silicon | `ptywright-aarch64-apple-darwin.tar.gz`      |
| macOS Intel         | `ptywright-x86_64-apple-darwin.tar.gz`       |
| Linux x86_64        | `ptywright-x86_64-unknown-linux-gnu.tar.gz`  |
| Linux aarch64       | `ptywright-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64      | `ptywright-x86_64-pc-windows-msvc.zip`       |

On macOS and Linux:

```bash
tar xzf ptywright-<target>.tar.gz
./ptywright --version
```

On Windows:

```powershell
Expand-Archive .\ptywright-x86_64-pc-windows-msvc.zip
.\ptywright.exe --version
```

## Install script

The install script targets macOS and Linux release archives:

```bash
curl -fsSL https://raw.githubusercontent.com/utensils/ptywright/main/install.sh | sh
```
