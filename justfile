# List available recipes.
default:
    @just --list

# Build the library and CLI with the default features.
build:
    cargo build --locked

# Format the crate and standalone interoperability harnesses.
fmt:
    #!/bin/sh
    set -eu
    for manifest in Cargo.toml interop/*/Cargo.toml; do
        cargo fmt --manifest-path "$manifest"
    done

# Check formatting and run strict Clippy on every Rust target.
lint:
    #!/bin/sh
    set -eu
    for manifest in Cargo.toml interop/*/Cargo.toml; do
        cargo fmt --manifest-path "$manifest" --check
        cargo clippy --locked --manifest-path "$manifest" --all-features --all-targets -- -D warnings
    done

# Test the protocol core, optional features and in-memory EEBUS peers.
test:
    cargo test --locked --no-default-features
    cargo check --locked --features mqtt --all-targets
    cargo test --locked --features eebus
    cargo check --locked --features eebus-pairing --all-targets
    cargo test --locked --all-features
    cargo test --locked --manifest-path interop/eebus/Cargo.toml

# Run lint and tests locally without a broker.
check: lint test

# Run a simulated power-limit exchange.
demo:
    cargo run --locked --example roundtrip

# Test MQTT and the authenticated gateway channel with Docker and OpenSSL.
broker:
    scripts/check-mqtt.sh --with-smgw

# Exercise both pinned OpenEnergyTwin peers with Docker, Git and uv.
peers:
    uv run --locked --python 3.13 --script interop/device-peer/check.py

# Verify recovery after process termination.
recovery:
    python3 interop/restart/check.py

# Check the guide and build it together with the Rust API reference.
site:
    zola --root site check --skip-external-links
    scripts/build-site.sh

# Preview the built site on loopback.
serve port="8000": site
    python3 -m http.server --bind 127.0.0.1 --directory site/public {{quote(port)}}
