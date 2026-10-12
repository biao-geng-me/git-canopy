#!/usr/bin/env bash
# Set up a development checkout: `canopy` and `git-canopy` in ~/.local/bin point at
# target/release/canopy, and a post-commit hook rebuilds it in the background after every commit,
# so the installed canopy always runs the latest commit. Re-running is harmless.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
HOOK=$(git rev-parse --git-path hooks/post-commit)
cat >"$HOOK" <<'HOOK'
#!/bin/sh
# Rebuild the release binary in the background after each commit (installed by
# scripts/install-dev.sh). Builds queue behind each other (flock); log in
# target/post-commit-build.log.
cd "$(git rev-parse --show-toplevel)" || exit 0
mkdir -p target
(
  flock 9
  # Hooks may run without ~/.cargo/bin on PATH.
  CARGO=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")
  "$CARGO" build --release >target/post-commit-build.log 2>&1
) 9>target/.post-commit-build.lock </dev/null >/dev/null 2>&1 &
HOOK
chmod +x "$HOOK"
PATH=$PATH:$HOME/.cargo/bin cargo build --release
mkdir -p ~/.local/bin
ln -sfn "$ROOT/target/release/canopy" ~/.local/bin/canopy
ln -sfn "$ROOT/target/release/canopy" ~/.local/bin/git-canopy
echo "installed: ~/.local/bin/canopy, ~/.local/bin/git-canopy -> $ROOT/target/release/canopy"
