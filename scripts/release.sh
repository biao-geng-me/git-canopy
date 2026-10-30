#!/usr/bin/env bash
# Build the static Linux release asset for the current version and this machine's architecture
# into target/dist: canopy-<arch>-unknown-linux-musl.tar.gz (canopy, README.md, LICENSE) and its
# .sha256. Run it once per architecture (x86_64, aarch64), each on a machine of that arch; the
# assets of earlier runs in target/dist are kept. Set CARGO_TARGET_DIR when the checkout is
# shared between machines of different architectures.
# Needs: rustup target add <arch>-unknown-linux-musl
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET=$(uname -m)-unknown-linux-musl
NAME=canopy-$TARGET
# Panic messages embed source paths: keep the build machine's home directory out of the binary.
export RUSTFLAGS="--remap-path-prefix=$HOME=~ --remap-path-prefix=$(pwd)=."
cargo build --release --locked --target "$TARGET"
BIN=${CARGO_TARGET_DIR:-target}/$TARGET/release/canopy
"$BIN" --version
if strings "$BIN" | grep -q "$HOME"; then
  echo "release: $BIN still contains $HOME" >&2
  exit 1
fi
STAGE=target/dist/$NAME
rm -rf "$STAGE" "target/dist/$NAME.tar.gz"* && mkdir -p "$STAGE"
cp "$BIN" README.md LICENSE "$STAGE/"
tar -czf "target/dist/$NAME.tar.gz" -C "$STAGE" canopy README.md LICENSE
(cd target/dist && sha256sum "$NAME.tar.gz" >"$NAME.tar.gz.sha256")
ls -l target/dist/*.tar.gz*
