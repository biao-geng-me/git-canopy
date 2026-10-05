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
  -i, --interval <SECS>   poll interval for worktrees that can't be watched (default: 60);
                          watched worktrees are re-checked every 10 minutes regardless
      --base <BRANCH>     branch to compare worktrees against (default: main, master, trunk,
                          develop, or the main worktree's branch)
      --layout <MODE>     stacked (graph above worktrees, default) or side
      --no-poll           never poll; update on file system events and `r` only
      --no-watch          disable file system notifications, poll only
      --no-herdr          don't show herdr agent activity
```

### Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | move, through expanded lines too (selecting a worktree jumps the graph to it) |
| `PgUp` `PgDn`, `Ctrl-u` `Ctrl-d`, `g` `G` | page, half page, first, last |
| `Space`, click | graph: expand / collapse a commit or `◌` row (message, files), or a file (its diff) |
| `→` `←` / `l` `h` | expand / collapse (from inside a diff, `←` folds the enclosing file, then the row) |
| `Esc` | fold the expansion under the cursor back to its row (on a folded row: fold everything) |
| `Enter`, double-click | full diff view, opened at the file and line under the cursor |
| `Tab` | switch between the graph, the worktree list and the shell (when one is open) |
| `v` | stacked / side-by-side layout |
| `a`, double-click an agent marker | herdr: jump to the worktree's agent pane |
| `s`, double-click a worktree path | open a shell in that worktree (see [Shells](#shells)) |
| `y` + key | copy to the clipboard (see [Copying](#copying)) |
| `r` | refresh now |
| `?` | help |
| `q`, `Ctrl-C` | quit |

`Space`, `→`, `←` and `Esc` also work from the worktree list: they act on that worktree's row in the
graph (its `◌` row when dirty, else its HEAD commit), and focus stays on the list.

Each worktree and branch is drawn in the color of its graph lane: the `[name]` and branch labels in
the graph and the worktree names in the list (remote branches are red). A dirty worktree is labelled
once, on its `◌` row.

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

## herdr agent activity

When a [herdr](https://herdr.dev) server is reachable, canopy shows which worktrees have a coding agent
in them, and above all which agent is **blocked waiting for you**. It works in any terminal, not only
inside herdr, and stays invisible when herdr isn't running (`--no-herdr` turns it off).

- Worktree rows get `◆ blocked` (bold red), `▶ working`, `✓ done` or `· idle` for their most urgent
  agent, plus `×N` when several agents share a worktree; the graph's `[name]` labels get the same mark.
- The footer sums it up: `herdr: 3 agents, 1 blocked`.
- `a` (or a double-click on the marker) asks herdr to focus that worktree's agent pane. That and
  opening a shell pane (below) are the only things canopy ever asks herdr to do; it never prompts,
  sends keys to, starts or stops agents.

An agent belongs to the worktree containing its working directory (deepest match), else to the worktree
of its herdr workspace. canopy connects to herdr's socket (`HERDR_SOCKET_PATH`, `HERDR_SESSION`, or
`~/.config/herdr/herdr.sock`), follows its event stream, and reconnects if herdr restarts.

## Shells

`s`, or a double-click on a worktree's path, opens a shell in that worktree, in the first of these that
works:

1. **A herdr pane** split below canopy's, when canopy runs inside herdr. It's an ordinary herdr pane
   (resize, move or zoom it like any other) and closes when you `exit` the shell.
2. **A tmux pane** split below canopy's, when canopy runs inside tmux; likewise closed by `exit`.
3. **An inset** in the lower half of the graph's space: a real terminal (your `$SHELL` on a pty).
   Keys go to the shell while it has focus; `Ctrl-]` hands them back to canopy, a click or `Tab`
   returns to it, and `exit` closes it. The mouse wheel scrolls its history. One inset shell at a time.

## Copying

`y` followed by a key copies something about the line under the cursor (or the open diff) to the
system clipboard; while `y` waits, the footer lists what is available there.

| Keys | Copies |
| --- | --- |
| `yy` | the obvious thing: the file on a file line, the sha on a commit, the path on a worktree |
| `yh` / `ys` | full / short commit sha (a worktree's HEAD) |
| `yb` | branch name |
| `ym` / `yM` | commit subject / full message |
| `yp` | worktree path |
| `yf` | file path (on a file, or the file open in the diff view) |
| `yd` | the diff, as a patch `git apply` accepts: the commit, the worktree's uncommitted changes (untracked files included), or just the file under the cursor |

The text goes to a local clipboard program when there is one (`clip.exe` on WSL, `pbcopy`,
`wl-copy`, `xclip`, `xsel`), else into tmux's buffer (`tmux load-buffer -w`, which tmux passes on to
the terminal's clipboard), else straight to the terminal as an OSC 52 escape sequence, which most
terminals support and which also works over SSH.

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
cargo run --example herdr -- . 30                      # what canopy sees from herdr (read-only calls)
```

See `CLAUDE.md` for the design notes.
