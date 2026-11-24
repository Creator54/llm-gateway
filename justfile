set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Build the gw release binary.
build:
    cd gw && nix shell nixpkgs#cargo nixpkgs#rustc -c cargo build --release

# Build, then link gw into ~/.local/bin.
install: build
    install -d ~/.local/bin
    ln -sf "{{justfile_directory()}}/gw/target/release/gw" ~/.local/bin/gw

# fmt, tests, clippy.
check:
    nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#clippy nixpkgs#rustfmt -c sh -c 'cd gw && cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings'
