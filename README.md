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
  were last edited, its HEAD commit (sha, age) and its path.
- **Graph** (top): the commit graph of all branches, remotes and tags, with authors. Each worktree's
  HEAD is labelled `[name]`, and a dirty worktree gets a `◌` "uncommitted" row right above its HEAD.
- **Expand in place** (graph): `Space` (or a click) on a commit or a `◌` row unfolds its full
  subject and message (wrapped) and its changed files right there; `Space` on a file unfolds its diff.
  `Esc` folds back. `Enter` (or a double-click) opens the full diff view.
- **Layout:** stacked by default; `v` (or `--layout side`) puts the worktrees beside the graph.
- **Live:** git operations (commits, checkouts, staging) show up within ~150ms via file system events
  on the git dir, and file edits within ~150ms via events on each worktree's directories (git-ignored
  directories like `target/` are not watched, and a busy worktree is re-checked at most every 2s).
  Polling is only a safety net: worktrees that can't be watched every 60s (`-i`), watched ones every
  10 minutes, spread out one worktree at a time. `--no-poll` turns it off; `r` refreshes now.
  The footer shows e.g. `watching 8/9 · poll 60s`. Refreshing never blocks the UI.
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
  -i, --interval <SECS>   status poll interval in seconds (default: 60)
      --base <BRANCH>     branch to compare worktrees against (default: main, master, trunk,
                          develop, or the main worktree's branch)
      --layout <MODE>     stacked (graph above worktrees, default) or side
      --no-poll           never poll; update on file system events and `r` only
      --no-watch          disable file system notifications, poll only
```

### Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | move, through expanded lines too (selecting a worktree jumps the graph to it) |
| `PgUp` `PgDn`, `Ctrl-u` `Ctrl-d`, `g` `G` | page, half page, first, last |
| `Space`, click | graph: expand / collapse a commit or `◌` row (message, files), or a file (its diff) |
| `→` `←` / `l` `h` | expand / collapse (from inside a diff, `←` folds the enclosing file, then the row) |
| `Esc` | fold the expansion under the cursor back to its row (on a folded row: fold everything) |

`Space`, `→`, `←` and `Esc` also work from the worktree list: they act on that worktree's row in the
graph (its `◌` row when dirty, else its HEAD commit), and focus stays on the list.

Each worktree and branch is drawn in the color of its graph lane: the `[name]` and branch labels in
the graph and the worktree names in the list (remote branches are gray). A dirty worktree is labelled
once, on its `◌` row.
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

- Where file system events don't work (WSL2's `/mnt/c`, other 9p/FUSE mounts; detected from
  `/proc/self/mountinfo`) everything is polled at `-i`. On network filesystems (NFS, Lustre, GPFS, …)
  events only report changes made from this machine, so agents on other nodes show up at the
  10-minute fallback poll (or `r`). Worktrees with over 10k directories are polled instead of watched.
- Cost of a poll: per worktree one `git status`, which stats every tracked file and lists the
  directories (metadata only, normally served from the kernel's cache, no file contents), plus two
  cheap repo-wide commands.
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
