#!/usr/bin/env bash
# Set up a development checkout: `canopy` and `git-canopy` in ~/.local/bin point at
# target/release/canopy, and a post-commit hook rebuilds it in the background after every commit,
# so the installed canopy always runs the latest commit. Re-running is harmless.
# CANOPY_BIN_DIR overrides ~/.local/bin (e.g. a per-arch bin dir when $HOME is shared by
# machines of different architectures).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
BIN=${CANOPY_BIN_DIR:-$HOME/.local/bin}
PATH=$PATH:$HOME/.cargo/bin
CARGO=$(command -v cargo)
HOOK=$(git rev-parse --git-path hooks/post-commit)
cat >"$HOOK" <<HOOK
#!/bin/sh
# Rebuild the release binary in the background after each commit (installed by
# scripts/install-dev.sh). Builds queue behind each other (flock); log in
# target/post-commit-build.log.
cd "\$(git rev-parse --show-toplevel)" || exit 0
mkdir -p target
(
  flock 9
  # Hooks may run without cargo on PATH: use the one found at install time.
  "\$(command -v cargo || echo '$CARGO')" build --release >target/post-commit-build.log 2>&1
) 9>target/.post-commit-build.lock </dev/null >/dev/null 2>&1 &
HOOK
chmod +x "$HOOK"
"$CARGO" build --release
mkdir -p "$BIN"
ln -sfn "$ROOT/target/release/canopy" "$BIN/canopy"
ln -sfn "$ROOT/target/release/canopy" "$BIN/git-canopy"
echo "installed: $BIN/canopy, $BIN/git-canopy -> $ROOT/target/release/canopy"
case ":$PATH:" in *":$BIN:"*) ;; *) echo "note: $BIN is not on PATH" ;; esac
