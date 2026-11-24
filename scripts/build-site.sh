#!/bin/sh
# Build the operator guide and API reference into site/public.
set -eu
cd "$(dirname "$0")/.."
zola --root site build
RUSTDOCFLAGS="-D warnings ${RUSTDOCFLAGS:-}" cargo doc --locked --all-features --no-deps
cargo_target=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')
mkdir -p site/public/api
cp -R "$cargo_target/doc/." site/public/api/
