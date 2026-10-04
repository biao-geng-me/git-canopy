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

- **Left:** one row per worktree: state marker, branch (or directory name when detached), staged `+`,
  modified `~`, untracked `?` and conflicted `!` counts, ahead/behind the base branch `↑↓` and the
  upstream `⇡⇣`, and in-progress operations (`MERGING`, `REBASING`, …).
- **Right:** the commit graph of all branches, remotes and tags. Each worktree's HEAD is labelled
  `[name]`, and a dirty worktree gets a `◌` "uncommitted" row right above its HEAD.
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
      --no-watch          disable file system notifications, poll only
```

### Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | move the selection (selecting a worktree jumps the graph to it) |
| `PgUp` `PgDn`, `Ctrl-u` `Ctrl-d`, `g` `G` | page, half page, first, last |
| `Tab`, `←` `→` / `h` `l` | switch between the worktree list and the graph |
| `Enter`, double-click | diff of the selected commit or worktree |
| `r` | refresh now |
| `?` | help |
| `q`, `Ctrl-C` | quit |

In the diff view: `↑` `↓` `PgUp` `PgDn` `Space` scroll, `n` / `p` jump to the next/previous file, `←` `→`
scroll sideways, `r` reloads, `Esc` or `q` goes back. A worktree diff reloads by itself when that
worktree's status changes.

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
