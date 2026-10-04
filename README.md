# canopy

A live-updating terminal view of every git worktree of a repo, the view from above the trees.

When several agents (or people) each work in their own linked worktree, canopy answers "what is every
tree doing, and which ones are dirty?" on one screen:

```
╭ worktrees 8 ✎4 ────────────────────╮╭ graph · base main ─────────────────────────────────────────╮
│● main        clean    ⇣2           ││  │ ◌ [feat-auth] +3 ~2 ?1 uncommitted                       │
│⚠ conflict    !1       ↑1 ↓1 MERGING││  │ ◉ 35d6da7  2h [feat-auth] add token refresh              │
│✎ feat-auth   +3 ~2 ?1 ↑2           ││  │ ● e1223a9  3h auth: login                                │
│✎ fix-parser  ~1       ↑1 ↓1        ││  │ │ ● eeb9f2a  1d (origin/main) upstream: more changelog   │
│● refactor-db clean    ↑4           ││◉─┴─╯ e7dc7c6  1d [main] readme: badges                    │
╰────────────────────────────────────╯╰────────────────────────────────────────────────────────────╯
```

- **Worktrees** (bottom): one row per worktree: state marker, branch (or directory name when detached),
  staged `+`, modified `~`, untracked `?` and conflicted `!` counts, ahead/behind the base branch `↑↓`
  and the upstream `⇡⇣`, in-progress operations (`MERGING`, `REBASING`, …), when its changed files
  were last edited, its HEAD commit (sha, age, subject) and its path.
- **Graph** (top): the commit graph of all branches, remotes and tags, with authors. Each worktree's
  HEAD is labelled `[name]`, and a dirty worktree gets a `◌` "uncommitted" row right above its HEAD.
- **Expand in place:** `Space` (or a click) on a commit or worktree unfolds its changed files right
  there; `Space` on a file unfolds its diff. `Enter` (or a double-click) opens the full diff view.
- **Layout:** stacked by default; `v` (or `--layout side`) puts the worktrees beside the graph.
- **Live:** status is polled every 1.5s, and changes to refs, HEADs and indexes are picked up via file
  system events within ~150ms. Refreshing never blocks the UI.
- **Details on demand:** `Enter` shows `git show` for a commit, or the uncommitted diff (including
  untracked files) for a worktree.

canopy is read-only. It never stages, commits or writes to the repository, and every git call runs with
optional locks disabled, so it never contends with an agent for `index.lock`.

## Install

Requires `git` (2.36 or newer) at runtime, and a Rust toolchain to build.

```sh
cargo install --path .            # installs the `canopy` binary
ln -s "$(command -v canopy)" ~/.local/bin/git-canopy   # optional: makes `git canopy` work
```

For a fully static Linux binary: `cargo build --release --target x86_64-unknown-linux-musl`.

## Usage

```
canopy [OPTIONS] [PATH]
git canopy [OPTIONS] [PATH]

  PATH                    any directory inside the repository (default: current directory)
  -i, --interval <SECS>   status poll interval in seconds (default: 1.5)
      --base <BRANCH>     branch to compare worktrees against (default: main, master, trunk,
                          develop, or the main worktree's branch)
      --layout <MODE>     stacked (graph above worktrees, default) or side
      --no-watch          disable file system notifications, poll only
```

### Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | move, through expanded lines too (selecting a worktree jumps the graph to it) |
| `PgUp` `PgDn`, `Ctrl-u` `Ctrl-d`, `g` `G` | page, half page, first, last |
| `Space`, click | expand / collapse a commit or worktree (its files), or a file (its diff) |
| `→` `←` / `l` `h` | expand / collapse (from inside a diff, `←` folds the enclosing file, then the row) |
| `Enter`, double-click | full diff view, opened at the file and line under the cursor |
| `Tab` | switch between the graph and the worktree list |
| `v` | stacked / side-by-side layout |
| `r` | refresh now |
| `?` | help |
| `q`, `Ctrl-C` | quit |

The diff view lists the changed files on top (status, path, `+`/`-` counts; the first entry is the
commit message and diffstat, or a summary for a worktree) and shows the selected file's diff below.
`↑` `↓` pick a file, `Enter` or `Tab` moves into the diff where `↑` `↓` scroll, `Esc` or `Tab` goes back
to the list. Anywhere: `n` / `p` next/previous file, `PgUp` `PgDn` `Space` scroll the diff, `←` `→` scroll
sideways, `r` reloads, `q` goes back to the overview. A worktree diff reloads by itself when that
worktree's status changes, keeping the selected file and scroll position.

The mouse works too: click to select, wheel to scroll.

### Markers

| Marker | Meaning |
| --- | --- |
| `●` | clean |
| `✎` | uncommitted changes |
| `⚠` | conflicts or an operation in progress (merge, rebase, cherry-pick, …) |
| `✗` | prunable: the worktree directory is gone |
| `…` | status still loading |

In the graph, `◉` marks a commit that is some worktree's HEAD, `○` a merge commit and `◌` uncommitted
changes.

## Notes

- On WSL2, repositories under `/mnt/c` don't deliver file system events. canopy notices nothing and
  keeps polling, so updates just take up to one interval.
- The graph loads the newest 1000 commits first and loads more as you scroll toward the end.

## Development

```sh
cargo test                       # unit tests + integration tests against a generated fixture repo
cargo clippy --all-targets
scripts/mkfixture.sh [DIR]       # throwaway repo with worktrees in every interesting state
scripts/mkbigrepo.sh [DIR]       # 20k commits, 15 worktrees, for performance checks
cargo run --release --example startup -- DIR/repo   # time to first worktree list / statuses / graph
```

See `CLAUDE.md` for the design notes.
