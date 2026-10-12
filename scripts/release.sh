#!/usr/bin/env bash
# Build the static Linux release asset for the current version into target/dist:
#   canopy-x86_64-unknown-linux-musl.tar.gz (canopy, README.md, LICENSE) and its .sha256.
# Needs: rustup target add x86_64-unknown-linux-musl
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET=x86_64-unknown-linux-musl
NAME=canopy-$TARGET
cargo build --release --locked --target "$TARGET"
BIN=target/$TARGET/release/canopy
"$BIN" --version
STAGE=target/dist/$NAME
rm -rf target/dist && mkdir -p "$STAGE"
cp "$BIN" README.md LICENSE "$STAGE/"
tar -czf "target/dist/$NAME.tar.gz" -C "$STAGE" canopy README.md LICENSE
(cd target/dist && sha256sum "$NAME.tar.gz" >"$NAME.tar.gz.sha256")
ls -l target/dist/*.tar.gz*
