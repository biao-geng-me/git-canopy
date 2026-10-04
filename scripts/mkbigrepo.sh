#!/usr/bin/env bash
# Build a large throwaway repo for performance checks: a long main branch plus agent branches,
# each checked out in its own worktree, some of them dirty.
#
#   scripts/mkbigrepo.sh [DIR] [COMMITS] [WORKTREES]     (defaults: mktemp -d, 20000, 15)
#
# Then e.g.: cargo run --release --example startup -- DIR/repo
set -euo pipefail

DIR=${1:-$(mktemp -d -t canopy-big.XXXXXX)}
N=${2:-20000}
WT=${3:-15}
mkdir -p "$DIR"
DIR=$(cd "$DIR" && pwd)
if [ -n "$(ls -A "$DIR")" ]; then
  echo "mkbigrepo: $DIR is not empty" >&2
  exit 1
fi
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null

git init -q -b main "$DIR/repo"
awk -v N="$N" -v WT="$WT" 'BEGIN {
  srand(1); t = 1700000000; mark = 0
  for (i = 0; i < N; i++) {
    main[i] = commit("main", (i ? main[i-1] : 0), "f" (i % 500) ".txt", i)
  }
  for (b = 0; b < WT; b++) {
    p = main[N - 1 - int(rand() * (N < 3000 ? N : 3000))]
    n = 5 + int(rand() * 25)
    for (j = 0; j < n; j++) p = commit("agent-" b, p, "agent" b ".txt", j)
  }
}
function commit(branch, parent, path, i,    msg, data) {
  data = "line " i "\n"
  printf "blob\nmark :%d\ndata %d\n%s\n", ++mark, length(data), data
  msg = "commit " branch " " i "\n"
  printf "commit refs/heads/%s\nmark :%d\ncommitter Big <big@example.com> %d +0000\ndata %d\n%s", \
    branch, ++mark, (t += 60), length(msg), msg
  if (parent) printf "from :%d\n", parent
  printf "M 100644 :%d %s\n\n", mark - 1, path
  return mark
}' | git -C "$DIR/repo" fast-import --quiet
git -C "$DIR/repo" reset -q --hard main

for ((b = 0; b < WT; b++)); do
  git -C "$DIR/repo" worktree add -q "$DIR/wt/agent-$b" "agent-$b"
  if ((b % 3 == 1)); then
    echo dirty >>"$DIR/wt/agent-$b/agent$b.txt"
  fi
done
echo "$DIR"
