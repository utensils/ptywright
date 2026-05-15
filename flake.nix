{
  description = "ptywright — drive interactive terminal applications through PTYs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.devshell.flakeModule
        inputs.treefmt-nix.flakeModule
      ];

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];

      perSystem =
        {
          system,
          lib,
          ...
        }:
        let
          pkgs = import inputs.nixpkgs {
            localSystem = system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rustfmt"
              "clippy"
              # llvm-tools-preview ships the `llvm-cov` and `llvm-profdata`
              # binaries that match this rustc release. Without it,
              # `cargo llvm-cov` falls back to whatever LLVM is on PATH,
              # which can mismatch the .profraw format produced by tests.
              "llvm-tools-preview"
            ];
          };

          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;
          src = craneLib.path ./.;

          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

          ptywrightMeta = {
            description = cargoToml.package.description;
            homepage = cargoToml.package.homepage;
            license = lib.licenses.mit;
            mainProgram = "ptywright";
            maintainers = [
              {
                name = "James Brink";
                email = "brink.james@gmail.com";
                github = "jamesbrink";
                githubId = 28793;
              }
            ];
            platforms = lib.platforms.unix;
          };

          commonArgs = {
            inherit src;
            pname = cargoToml.package.name;
            version = cargoToml.package.version;
            strictDeps = true;
            meta = ptywrightMeta;
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          ptywright = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
            }
          );
        in
        {
          _module.args.pkgs = pkgs;

          packages = {
            inherit ptywright;
            default = ptywright;
          };

          apps.default = {
            type = "app";
            program = "${ptywright}/bin/ptywright";
            meta = ptywrightMeta;
          };

          devshells.default = {
            motd = ''
              {202}ptywright{reset} — drive interactive terminal applications through PTYs ({bold}${system}{reset})
              $(type menu &>/dev/null && menu)
            '';

            packages = [
              rustToolchain
              pkgs.rust-analyzer
              pkgs.cargo-llvm-cov
              pkgs.git
              pkgs.gh
              pkgs.jq
              pkgs.bun
            ];

            env = [
              {
                name = "RUST_BACKTRACE";
                value = "1";
              }
            ];

            commands = [
              {
                category = "build";
                name = "build";
                help = "cargo build (debug)";
                command = "cargo build \"$@\"";
              }
              {
                category = "build";
                name = "build-release";
                help = "cargo build --release";
                command = "cargo build --release \"$@\"";
              }
              {
                category = "check";
                name = "check";
                help = "cargo check";
                command = "cargo check \"$@\"";
              }
              {
                category = "check";
                name = "clippy";
                help = "cargo clippy -- -D warnings (matches CI)";
                command = "cargo clippy \"$@\" -- -D warnings";
              }
              {
                category = "check";
                name = "fmt";
                help = "cargo fmt";
                command = "cargo fmt \"$@\"";
              }
              {
                category = "check";
                name = "fmt-check";
                help = "cargo fmt --check (matches CI)";
                command = "cargo fmt --check \"$@\"";
              }
              {
                category = "check";
                name = "run-tests";
                help = "cargo test (matches CI)";
                command = "cargo test \"$@\"";
              }
              {
                category = "check";
                name = "ci-local";
                help = "run the same sequence CI runs: fmt-check, check, clippy, test, build (default + --no-default-features)";
                command = ''
                  set -euo pipefail
                  cargo fmt --all -- --check
                  cargo check
                  cargo clippy -- -D warnings
                  cargo test
                  cargo build --release
                  # Lean baseline: keep the `--no-default-features` path
                  # green so callers who opt out of the `repl` stack don't
                  # silently break. Mirrors the extra steps in
                  # .github/workflows/ci.yml.
                  cargo check --no-default-features
                  cargo clippy --no-default-features -- -D warnings
                  cargo test --no-default-features
                '';
              }
              {
                category = "check";
                name = "coverage";
                help = "test coverage summary (pass --html for a browsable report)";
                command = ''
                  set -euo pipefail
                  # llvm-tools-preview is part of the rustToolchain extensions
                  # so cargo-llvm-cov finds llvm-cov / llvm-profdata on PATH
                  # at versions matched to this rustc release. No more env
                  # shimming via `find /nix/store` (which picked random
                  # LLVM majors and broke after store GC).
                  if [ "''${1:-}" = "--html" ]; then
                    cargo llvm-cov --workspace --html --output-dir target/coverage
                    echo "Report: target/coverage/html/index.html"
                  else
                    cargo llvm-cov --workspace --summary-only
                  fi
                '';
              }
              {
                category = "run";
                name = "ptywright";
                help = "run ptywright (with the default `repl` feature, including the interactive REPL client)";
                command = "cargo run --features repl -- \"$@\"";
              }
              {
                category = "docs";
                name = "docs-dev";
                help = "start the VitePress dev server for the docs site";
                command = "cd website && bun install && bun run dev \"$@\"";
              }
              {
                category = "docs";
                name = "docs-build";
                help = "build the documentation site (static output in website/.vitepress/dist)";
                command = "cd website && bun install && bun run build";
              }
              {
                category = "docs";
                name = "docs-preview";
                help = "preview the built documentation site";
                command = "cd website && bun run preview \"$@\"";
              }
              {
                category = "docs";
                name = "docs-fmt";
                help = "format documentation with prettier";
                command = "cd website && bun run fmt";
              }
              {
                category = "docs";
                name = "docs-fmt-check";
                help = "check documentation formatting (matches CI)";
                command = "cd website && bun run fmt:check";
              }
            ];
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs.nixfmt.enable = true;
            programs.rustfmt = {
              enable = true;
              edition = "2024";
            };
          };
        };
    };
}
