---
applyTo: "flake.nix,.github/workflows/**,rust-toolchain.toml,Cargo.toml,install.sh"
---

# Build, packaging, CI — review constraints

## Nix devshell

- `flake.nix` uses `flake-parts` + `numtide/devshell` + `treefmt-nix`. Don't switch to another flake stack.
- Devshell commands are categorised (`build`, `check`, `run`, `docs`). New commands declare a category.
- `ci-local` is the canonical pre-push gate: fmt-check → check → clippy → test → build → unittest. CI runs the equivalent on Linux / macOS / Windows.
- The Python suite in `ci-local` is **stdlib `unittest`**, NOT pytest. The label / help text should say "unittest". The script uses `python -m unittest discover ...`.

## CI workflows

- `.github/workflows/ci.yml` exercises both feature lanes (default with `repl`, and `--no-default-features`) across Linux / macOS / Windows. Windows runs check-only on the no-default-features lane; macOS runs check+test; Linux runs check+clippy+test+coverage.
- `actions/setup-python@v6` installs `python` (not `python3`) on every runner — the launcher is uniform across OSes. Don't add platform-specific Python launcher branches.
- The `_test-fixtures` feature is required for `cargo test`. CI passes it. Bare `cargo install ptywright` does not get the fixture, by design.
- The `--locked` flag is required on CI's Cargo invocations. Mirror it when reproducing CI failures locally.

## Cargo

- MSRV is pinned in `rust-toolchain.toml`. Don't bump it casually — coordinate with maintainer.
- `mlua` uses the `vendored` feature so source builds outside Nix need a C compiler. Don't propose switching to `lua-system`.
- The `repl` feature is on by default but the lean build (`--no-default-features`) must also work. CI exercises both.

## Packaging

- `install.sh` is the curl-able installer for prebuilt release artifacts. It supports Linux, macOS, and Windows (via PowerShell extraction).
- Release tooling promotes `[Unreleased]` in `CHANGELOG.md` to a versioned section on tag. Leave `[Unreleased]` populated; don't manually rename it.

## Review anti-patterns

- **"Use `python3`"** — Windows runners may not have it under that name. Use `python` and let `setup-python` provide it.
- **"Drop the `--locked` flag locally"** — it's required to reproduce CI exactly. Local mirror should match.
- **"Add a `make` wrapper"** — `make` adds a layer atop Cargo / the devshell. Use one of those directly.
- **"Switch to GitHub Actions matrix for Lua versions"** — the plugin runtime is Lua 5.4 only (mlua vendored). No matrix needed.
- **"Add Docker build steps"** — out of scope. Nix flake is the cross-platform packaging story.
