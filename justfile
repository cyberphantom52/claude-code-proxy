# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

# List available commands
default:
    @just --list

# Check Rust formatting
format:
    cargo fmt --all -- --check

# Check clippy
clippy:
    cargo clippy --all-targets -- -D warnings

# Check the build
build:
    cargo build --all

# Install release binary globally
install:
    cargo install --offline --path . --locked

# Install debug binary globally via symlink
install-dev:
    cargo build && ln -sf $(pwd)/target/debug/claude-code-proxy ~/.cargo/bin/claude-code-proxy

# Run the application
run *ARGS:
    cargo run -- "$@"

# Build and open the monitor demo in a running CuaBot session
cua-monitor-demo session:
    scripts/cua-monitor-demo '{{session}}'

# Internal release helper
_release bump *ARGS:
    @cargo-release {{bump}} {{ARGS}}

# Release a new patch version
release *ARGS:
    @just _release patch --skip-publish {{ARGS}}
