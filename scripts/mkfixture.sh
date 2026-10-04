#!/usr/bin/env bash
# Build a throwaway repo with several worktrees in various states, for manual and automated testing.
#
#   scripts/mkfixture.sh [DIR]
#
# DIR must not exist or be empty; defaults to a fresh `mktemp -d`. Prints DIR on success.
# Layout:
#   DIR/origin.git        bare "upstream" (main is 2 commits ahead of the local main)
#   DIR/repo              main worktree, branch main, behind upstream by 2, clean
#   DIR/wt/feat-auth      2 commits ahead of main; 3 staged, 2 modified, 1 untracked
#   DIR/wt/fix-parser     branched from main~1, 1 commit; 1 modified file
#   DIR/wt/refactor-db    4 commits ahead of main, clean, locked
#   DIR/wt/detached       detached HEAD at main~2, clean
#   DIR/wt/untracked-only branch docs at main; untracked files only
#   DIR/wt/conflict       mid-merge with an unmerged README.md
#   DIR/wt/gone           deleted from disk -> prunable
set -euo pipefail

DIR=${1:-$(mktemp -d -t canopy-fixture.XXXXXX)}
mkdir -p "$DIR"
DIR=$(cd "$DIR" && pwd)
if [ -n "$(ls -A "$DIR")" ]; then
  echo "mkfixture: $DIR is not empty" >&2
  exit 1
fi

# Hermetic: ignore the user's git config, fixed identity and increasing dates.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME="Fixture Bot" GIT_AUTHOR_EMAIL=bot@example.com
export GIT_COMMITTER_NAME="Fixture Bot" GIT_COMMITTER_EMAIL=bot@example.com
T=1700000000
tick() {
  T=$((T + 3600))
  export GIT_AUTHOR_DATE="@$T +0000" GIT_COMMITTER_DATE="@$T +0000"
}
# commit DIR FILE LINE MSG: append LINE to FILE, stage it, commit.
commit() {
  mkdir -p "$(dirname "$1/$2")"
  printf '%s\n' "$3" >>"$1/$2"
  git -C "$1" add "$2"
  tick
  git -C "$1" commit -q -m "$4"
}

R=$DIR/repo
W=$DIR/wt

git init -q -b main "$R"
commit "$R" README.md "# fixture" "initial commit"
commit "$R" src/main.rs "fn main() {}" "add main.rs"
tick
git -C "$R" tag -a v0.1 -m "release v0.1"

git -C "$R" switch -q -c feature-x
commit "$R" src/lib.rs "pub fn lib() {}" "add lib"
commit "$R" src/lib.rs "pub fn helper() {}" "lib: helpers"
git -C "$R" switch -q main
commit "$R" README.md "usage: run it" "readme: usage"
tick
git -C "$R" merge -q --no-ff feature-x -m "Merge branch 'feature-x'"
git -C "$R" branch -q -d feature-x

git -C "$R" switch -q -c old-experiment HEAD~1
commit "$R" notes.txt "abandoned idea" "experiment: try something"
git -C "$R" switch -q main
commit "$R" README.md "badges here" "readme: badges"

# Upstream that is 2 commits ahead of local main.
git init -q --bare -b main "$DIR/origin.git"
git -C "$R" remote add origin "$DIR/origin.git"
git -C "$R" push -q -u origin main
UP=$DIR/.upstream-clone
git clone -q -b main "$DIR/origin.git" "$UP"
commit "$UP" CHANGELOG.md "- upstream fix" "upstream: changelog"
commit "$UP" CHANGELOG.md "- upstream fix 2" "upstream: more changelog"
git -C "$UP" push -q origin main
rm -rf "$UP"
git -C "$R" fetch -q origin

# Worktrees.
git -C "$R" worktree add -q -b feat-auth "$W/feat-auth" main
commit "$W/feat-auth" src/auth.rs "pub fn login() {}" "auth: login"
commit "$W/feat-auth" src/auth.rs "pub fn refresh() {}" "add token refresh"
printf 'pub struct Token;\n' >"$W/feat-auth/src/token.rs"
printf 'pub struct Session;\n' >"$W/feat-auth/src/session.rs"
printf 'mod auth;\n' >>"$W/feat-auth/src/main.rs"
git -C "$W/feat-auth" add src/token.rs src/session.rs src/main.rs
printf '// todo\n' >>"$W/feat-auth/src/lib.rs"
printf '// wip\n' >>"$W/feat-auth/src/auth.rs"
printf 'scratch\n' >"$W/feat-auth/notes.md"

git -C "$R" worktree add -q -b fix-parser "$W/fix-parser" main~1
commit "$W/fix-parser" src/parser.rs "fn parse() {}" "handle eof"
printf '// fixme\n' >>"$W/fix-parser/src/parser.rs"

git -C "$R" worktree add -q -b refactor-db "$W/refactor-db" main
for i in 1 2 3 4; do
  commit "$W/refactor-db" src/db.rs "// step $i" "db: refactor step $i"
done
git -C "$R" worktree lock --reason "agent running" "$W/refactor-db"

git -C "$R" worktree add -q --detach "$W/detached" main~2

git -C "$R" worktree add -q -b docs "$W/untracked-only" main
mkdir -p "$W/untracked-only/docs"
printf '# Guide\n\nHello.\n' >"$W/untracked-only/docs/guide.md"
printf 'draft\n' >"$W/untracked-only/TODO"

git -C "$R" worktree add -q -b conflict "$W/conflict" main~1
commit "$W/conflict" README.md "conflicting line" "readme: conflicting edit"
tick
git -C "$W/conflict" merge -q main >/dev/null 2>&1 || true

git -C "$R" worktree add -q -b gone "$W/gone" main
rm -rf "$W/gone"

echo "$DIR"
