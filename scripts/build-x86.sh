#!/usr/bin/env bash
# Build a static x86_64 canopy of the committed HEAD on the sporc cluster (RIT RC), for a $HOME
# shared with the aarch64 tigris cluster. Submits a debug-partition job and returns; the job builds
# in the node's /tmp with the x86_64 toolchain in ~/sw/x86_64 and replaces
# target/x86_64/x86_64-unknown-linux-musl/release/canopy (what ~/sw/x86_64/bin/canopy points at).
# Log: target/x86_64/build.log. Called by the post-commit hook when sbatch is available.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
REV=$(git rev-parse --short HEAD)
# The export has no .git: hand build.rs the commit it is built from.
BUILD_REV=$(git log -1 --format='%h %cs' "$REV")
OUT=$ROOT/target/x86_64
mkdir -p "$OUT"
# --export=NONE: an aarch64 PATH/CARGO_HOME inherited from tigris would pick the Arm toolchain.
sbatch --parsable -M sporc -p debug -A flowlab -q qos_tier3 -J canopy-x86 -c 8 --mem=8G -t 20 \
  --export=NONE -o "$OUT/build.log" <<JOB
#!/bin/bash
set -euo pipefail
export CARGO_HOME=\$HOME/sw/x86_64/cargo RUSTUP_HOME=\$HOME/sw/x86_64/rustup
export PATH=\$CARGO_HOME/bin:/usr/local/bin:/usr/bin:/bin
export CANOPY_BUILD_REV="$BUILD_REV"
SRC=\$(mktemp -d /tmp/canopy-x86.XXXXXX)
trap 'rm -rf "\$SRC"' EXIT
git -C "$ROOT" archive $REV | tar -x -C "\$SRC"
cd "\$SRC"
CARGO_TARGET_DIR=\$SRC/target cargo build --release --target x86_64-unknown-linux-musl
BIN=$OUT/x86_64-unknown-linux-musl/release
mkdir -p "\$BIN"
# Copy then rename, so a canopy running from the old binary is untouched.
cp target/x86_64-unknown-linux-musl/release/canopy "\$BIN/.canopy.new"
mv -f "\$BIN/.canopy.new" "\$BIN/canopy"
echo "built $REV"
JOB
