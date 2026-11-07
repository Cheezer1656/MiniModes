#!/usr/bin/env bash
# Cross-compiles release binaries for the OCI Ampere A1 host (aarch64).
# Requires: rustup target add aarch64-unknown-linux-gnu; cargo install cargo-zigbuild; zig; cmake
# (aws-lc-sys, via chunkedge_network → reqwest → rustls, builds C with zig cc).
# Alternative: run `cargo build --release` natively on the A1 host.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=aarch64-unknown-linux-gnu
BINS=(lobby gamehost)

cargo zigbuild --release --target "$TARGET" $(printf -- '-p %s ' "${BINS[@]}")

mkdir -p dist/bin
for bin in "${BINS[@]}"; do
  cp "target/$TARGET/release/$bin" dist/bin/
done
echo "binaries in dist/bin"
