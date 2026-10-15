# canopy

*Many trees, one canopy.*

A live-updating terminal view of every git worktree of a repo, the view from above the trees.

When several agents (or people) each work in their own linked worktree, canopy answers "what is every
tree doing, and which ones are dirty?" on one screen:

```
── graph · base main ───────────────────────────────────────────────────────────────────────
 ◉─╮ dd701d0  5m [main] Merge branch 'fix-typo'
 │ ◉ 863b572  1h [fix-typo]⌫  readme: fix typo
 │ │ ◉ 2d7b362 12m [refactor-db] db: refactor step 4
 │ │ ● 7762531 25m db: refactor step 3
 │ │ ● d32a55b 40m db: refactor step 2
 │ │ ● 9dfe097  1h db: refactor step 1
 │ │ │ ◌ [fix-parser] ~1 uncommitted
 │ │ │ ◉ 253406e  2h handle eof
 │ │ │ │ ◌ [feat-auth] +3 ~2 ?1 uncommitted
 │ │ │ │ ◉ 35d6da7 20m add token refresh
 │ │ │ │ ● e1223a9  2h auth: login
 │ │ │ │ │ ● eeb9f2a  1d (origin/main) upstream: more changelog
 │ │ │ │ │ ● 1e5d2eb  1d upstream: changelog
 │ │ │ │ │ │ ◌ [docs]⌫  ?2 uncommitted
── worktrees 7 ✎ 3 ─────────────────────────────────────────────────────────────────────────
 ●  main        clean    ⇡2⇣2                   dd701d0  5m                            repo
 ⌫  detached    clean    ↓6                     86a259f  3d                     wt/detached
 ✎  feat-auth   +3 ~2 ?1 ↑2 ↓2   +7  edited 8s  35d6da7 20m                    wt/feat-auth
 ✎  fix-parser  ~1       ↑1 ↓3   +2  edited 3m  253406e  2h                   wt/fix-parser
 ⌫  fix-typo    clean    merged                 863b572  1h                     wt/fix-typo
 ●  refactor-db clean    ↑4 ↓2   +4             2d7b362 12m                  wt/refactor-db
 ⌫  docs        ?2       ↓2          edited 1h  e7dc7c6  1d               wt/untracked-only
────────────────────────────────────────────────────────────────────────────────────────────
 ~/src/app/repo  ·  dd701d0e  ·  main → origin/main
 ↑↓ move  Space expand  Enter full diff  b vs base  t git TUI  Tab panel  v layout  y copy
```

- **Worktrees** (bottom): one row per worktree: state marker, branch (or directory name when detached),
  staged `+`, modified `~`, untracked `?` (new folders apart, `?2/`: git doesn't list their files; `--nerd-font` shows file/folder icons instead) and conflicted `!` counts, ahead/behind the base branch `↑↓`
  and the upstream `⇡⇣`, in-progress operations (`MERGING`, `REBASING`, …), how much it changed since
  it forked from the base branch (`+412 -38`, commits and uncommitted changes together), overlap
  warnings (`⚠ feat-x`, see below), when its changed files were last edited, its HEAD commit (sha, age)
  and its path.
- **Graph** (top): the commit graph of all branches, remotes and tags, with authors. Each worktree's
  HEAD is labelled `[name]`, and a dirty worktree gets a `◌` "uncommitted" row right above its HEAD.
- **Expand in place** (graph): `Space` (or a click on the selected line) on a commit or a `◌` row unfolds its full
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
  untracked files) for a worktree. `b` shows everything a worktree changed since it forked from the
  base branch, committed or not: what you'd review before merging an agent's work.
- **Overlaps:** `⚠ feat-x` on a worktree row means another worktree (or the base branch, since this one
  forked) changed some of the same files: a likely merge conflict, or two agents doing the same work.
  The footer lists the files; in the diff view they are marked `⚠ also in feat-x`. Only each branch's
  own work counts: what it got from the base branch, or from a branch it was built on, doesn't.
  `o` (or a double-click on the `⚠`) opens the details: a table with one row per shared file and a
  column per worktree (this one first) holding its `+a -d` for that file (`·`: didn't touch it).
  The selected file's diff pane shows what this worktree did to it, then what each other side did,
  each since its own work began, uncommitted changes included. It reloads while either side keeps editing; `yd` copies
  both sides' patches (to paste to an agent).
- **Git TUI on demand:** `t` opens lazygit (or gitui, tig, `--git-tui`) in the selected worktree, the
  same way `s` opens a shell, for when you do want to stage, commit or rebase.

canopy is read-only. It never stages, commits or writes to the repository, and every git call runs with
optional locks disabled, so it never contends with an agent for `index.lock`. For changes, `t` hands
you over to a git TUI.

## Install

Requires `git` (2.36 or newer) at runtime. Linux (including WSL2) is what canopy is built and tested
on; other Unixes may work.

**Prebuilt (Linux x86_64):** a fully static binary is attached to each
[release](https://github.com/biao-geng-me/git-canopy/releases):

```sh
curl -L https://github.com/biao-geng-me/git-canopy/releases/latest/download/canopy-x86_64-unknown-linux-musl.tar.gz \
  | tar -xz -C ~/.local/bin canopy
```

**From source** (needs a Rust toolchain):

```sh
cargo install --git https://github.com/biao-geng-me/git-canopy   # installs the `canopy` binary
```

Optional: `ln -s "$(command -v canopy)" ~/.local/bin/git-canopy` makes `git canopy` work.

For a fully static Linux binary from a checkout: `cargo build --release --target x86_64-unknown-linux-musl`.

## Usage

```
canopy [OPTIONS] [PATH...]
git canopy [OPTIONS] [PATH...]

  PATH                    any directory inside a repository (default: current directory);
                          several watch several repositories
  -i, --interval <SECS>   poll interval for worktrees that can't be watched (default: 60);
                          watched worktrees are re-checked every 10 minutes regardless
      --base <BRANCH>     branch to compare worktrees against (default: main, master, trunk,
                          develop, or the main worktree's branch)
      --layout <MODE>     stacked (graph above worktrees, default) or side
      --no-poll           never poll; update on file system events and `r` only
      --no-watch          disable file system notifications, poll only
      --no-herdr          don't show herdr agent activity
      --herdr-repos       also watch every repository a herdr agent or workspace is in
      --git-tui <CMD>     git TUI that `t` opens (default: $CANOPY_GIT_TUI, else the first of
                          lazygit, gitui, tig on PATH)
      --nerd-font         Nerd Font icons for untracked files and folders (default:
                          $CANOPY_NERD_FONT=1)
```

### Several repositories

`canopy ~/a ~/b` watches both. The worktree list groups each repository's worktrees under a header row (name, base branch, worktree and dirty counts, blocked agents), and the graph shows the repository of the selected row: moving the selection into another repository switches the graph, which keeps its own cursor and expanded rows for each. Paths inside the same repository count once. With one repository there are no header rows.

`canopy --herdr-repos` also watches every repository that a herdr agent works in or a herdr workspace is checked out in, so one canopy follows all your agents without listing their projects. Repositories appear as agents start in them and are dropped two minutes after their last agent and workspace are gone (repositories named on the command line always stay). Without a `PATH` it starts with the current directory's repository if there is one, else empty until herdr names one.

### Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | move, through expanded lines too (selecting a worktree jumps the graph to it) |
| `PgUp` `PgDn`, `Ctrl-u` `Ctrl-d`, `g` `G` | page, half page, first, last |
| `Space`, click the selected line | graph: expand / collapse a commit or `◌` row (message, files), or a file (its diff) |
| `→` `←` / `l` `h` | expand / collapse (from inside a diff, `←` folds the enclosing file, then the row) |
| `Esc` | fold the expansion under the cursor back to its row (on a folded row: fold everything) |
| `Enter`, double-click | full diff view, opened at the file and line under the cursor |
| `b` | diff vs base: everything the worktree changed since it forked from the base branch |
| `o`, double-click a `⚠` overlap | overlaps: what this worktree and each overlapping one did to their shared files |
| `Tab` | switch between the graph, the worktree list and the shell (when one is open) |
| `v` | stacked / side-by-side layout |
| `a`, double-click an agent marker | herdr: jump to the worktree's agent pane |
| `s`, double-click a worktree path | open a shell in that worktree (see [Shells](#shells)) |
| `t` | open a git TUI (lazygit, gitui, tig) in that worktree, like a shell |
| `Backspace` | clean up a `⌫` worktree: remove its checkout, its branch and its herdr workspace, after confirming (see below) |
| `Backspace` twice | the same without the confirm popup (every check still runs) |
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

The mouse works too: the line under the pointer is faintly highlighted; a click selects, a second click on the selected line expands or collapses it (on a worktree: its row in the graph), a double-click opens the full view; the wheel scrolls. Clicking a worktree always moves the graph to it, also when the graph cursor was moved elsewhere.

### Markers

| Marker | Meaning |
| --- | --- |
| `●` | clean |
| `✎` | uncommitted changes |
| `⚠` | conflicts or an operation in progress (merge, rebase, cherry-pick, …) |
| `⚠ name` (light red, after the counts) | overlaps: `name` changed some of the same files |
| `⌫` (green) | safe to clean up: its work is in the base branch (`merged`), nothing uncommitted, no agent busy |
| `⌫` (gray) | safe to clean up: no commits of its own (HEAD is in the base branch, e.g. a fresh or a detached test checkout), nothing uncommitted, and git hasn't used it for an hour |
| `⌫` (yellow) | the same, but it has untracked files that cleaning up would lose |
| `✗` | prunable: the worktree directory is gone |
| `…` | status still loading |

In the graph, `◉` marks a commit that is some worktree's HEAD, `○` a merge commit and `◌` uncommitted
changes.

`merged` (where `↑a ↓b` would be) means the worktree's work is already in the base branch: its HEAD
is in base after commits of its own, or every file it changed has the same content in base
(squash-merged, rebased or cherry-picked). A worktree whose HEAD is in base without commits of its
own has nothing committed to lose either, so it can be cleaned up too (gray `⌫`): canopy can't tell
whether such a tree is finished or not started, only that removing it loses no commits. `⌫`
additionally needs a clean tree, no agent other than idle/done in it, and a worktree that could be
removed at all (not the main one, not on the base branch, not locked, not the one canopy runs in).

Two more checks guard against agents canopy can't see, such as one in a herdr on another machine that shares the filesystem, or one that hasn't changed anything yet. A worktree without commits of its own isn't offered while git has used it in the last hour, since it may be an agent's tree that was just set up. And a checkout in herdr's worktree directory (`~/.herdr/worktrees` unless herdr's config says otherwise) is only cleaned up when the herdr canopy talks to has a workspace for it.

### Cleaning up

`Backspace` on a `⌫` worktree (its row, or its `[name]` or `◌` row in the graph) removes it, after a
popup that says exactly what will happen and waits for `y`, `Enter` or `Backspace` (`Esc` cancels).
A quick double tap of `Backspace` skips the popup:

- if a herdr workspace is backed by that checkout, herdr's `worktree.remove` deletes the checkout and
  closes the workspace with its panes; otherwise `git worktree remove`;
- ignored files in the checkout (`target/`, `node_modules/`, `.env`, …) go with it, and the popup
  lists them;
- then the branch is deleted, if it still points at the commit that was checked. The footer prints
  the sha and the `git branch <name> <sha>` that restores it.

Everything is checked again right before anything is removed: a clean tree (no untracked files
either), the same HEAD, its work in the base branch, no busy agent. Nothing is forced: a worktree with
untracked files is refused (delete them first, `s` opens a shell there), and git itself refuses a
dirty checkout. Remote branches are never touched. On a worktree that isn't `⌫`, `Backspace` just says why
not. This is the only thing canopy ever writes to a repository.

## herdr agent activity

When a [herdr](https://herdr.dev) server is reachable, canopy shows which worktrees have a coding agent
in them, and above all which agent is **blocked waiting for you**. It works in any terminal, not only
inside herdr, and stays invisible when herdr isn't running (`--no-herdr` turns it off).

- Worktree rows get `◆ blocked` (bold red), `▶ working`, `✓ done` or `· idle` for their most urgent
  agent, plus `×N` when several agents share a worktree; the graph's `[name]` labels get the same mark.
- The footer sums it up: `herdr: 3 agents, 1 blocked`.
- `a` (or a double-click on the marker) asks herdr to focus that worktree's agent pane. That,
  opening a shell or git TUI pane (below) and removing a worktree's workspace when you clean it up
  (above) are the only things canopy ever asks herdr to do; it never prompts, sends keys to, starts
  or stops agents.

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

`t` opens a git TUI the same way: the first of lazygit, gitui and tig found on PATH, or the command
given with `--git-tui` / `CANOPY_GIT_TUI` (e.g. `"tig --all"`). Its pane closes when you quit it.

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
scripts/install-dev.sh           # canopy/git-canopy in ~/.local/bin (or $CANOPY_BIN_DIR) -> target/release, rebuilt after each commit
scripts/release.sh               # static release tarball + sha256 in target/dist
```

See `CLAUDE.md` for the design notes.
