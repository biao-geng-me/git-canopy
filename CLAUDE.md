# canopy

A live-updating TUI that shows every git worktree of a repo at a glance — the view from above the trees.

## Why it exists

In agentic development many agents work in parallel, each in its own linked worktree. You lose track of
what each tree is doing. Re-running `git log --oneline --graph --all` plus `git status` in every worktree
is tedious, and neither command shows the whole picture.

Existing tools each cover a piece, none covers the whole:
- gwq / lazyworktree / worktrunk: worktree table with dirty + ahead/behind, but no commit graph.
- keifu / gwm-cli: graph with an "uncommitted" row, but single worktree or manual refresh.
- lazygit: worktree panel without dirty state, 10s polling, graph not annotated per worktree.
- herdr (the multiplexer the author uses): sidebar shows only branch + ahead/behind.

**canopy's unique core:** an `--all` commit graph where each worktree's HEAD is labelled and gets a
synthetic "uncommitted changes" node when dirty, kept live automatically, with diffs one keypress away.

This is not another general git client. No staging, committing, rebasing, etc. Read-only monitoring.

## Requirements

- **Self-contained:** single static binary; the only runtime dependency is the `git` executable.
  Shell out to git — no libgit2/gix. (Matches whatever the user's git sees, config included.)
- **Fast:** startup feels instant on repos with thousands of commits and ~10–20 worktrees; refresh never
  blocks input.
- **Overview first, details on demand:** the main screen answers "what is every tree doing, and which are
  dirty?" Drill-down (diff, full commit message) via keyboard and mouse.
- **Never interfere with agents:** every git invocation uses `--no-optional-locks` (or
  `GIT_OPTIONAL_LOCKS=0`) so `status` never grabs `index.lock` while an agent commits. Never write to the repo.

## Stack

Rust + ratatui + crossterm. Keep dependencies lean (ratatui, crossterm, notify for fs events, maybe
anyhow). No async runtime unless clearly needed — threads + channels are fine.

Crate name `git-canopy` (crates.io `canopy` is taken); binary `canopy`. Also usable as `git canopy` via a
`git-canopy` symlink/copy on PATH.

## Target layout

```
┌ worktrees ────────────────┬ graph (all branches) ─────────────────────┐
│ ● main         ↓2   clean │ ◌ [feat-auth]  +3 ~2 ?1  uncommitted      │
│ ✎ feat-auth    +3 ~2 ?1   │ * a1b2 (feat-auth) add token refresh      │
│ ✎ fix-parser   ~1         │ │ ◌ [fix-parser]  ~1                      │
│ ● refactor-db  ↑4         │ │ * c3d4 (fix-parser) handle eof          │
│                           │ |/                                        │
│                           │ * 9f8e (main) merge #12                   │
└───────────────────────────┴───────────────────────────────────────────┘
 Enter: diff (commit or worktree)   Tab: switch focus   q: quit
```

- Left: one row per worktree — dirty marker, branch (or detached sha), staged/modified/untracked counts,
  ahead/behind vs upstream and/or vs the main branch. Selecting a worktree jumps the graph to its HEAD.
- Right: graph of all refs, newest first, with lanes drawn in box/unicode characters. Worktree HEADs are
  labelled `[name]`; dirty worktrees get a `◌` pseudo-row above their HEAD.
- Enter on a commit → `git show --stat -p`; on a `◌` row or dirty worktree → working-tree diff
  (`git diff HEAD` + untracked files listed/shown). Scrollable diff view with basic +/- coloring, Esc back.
- Mouse: click to select, wheel to scroll.

## Git facts to build on (git ≥ 2.43 on the dev machine)

- `git worktree list --porcelain -z` → worktree path, HEAD, branch, detached/locked/prunable.
- `git -C <wt> --no-optional-locks status --porcelain=v2 --branch -z` → `# branch.oid/head/upstream/ab`
  headers + entries (`1`/`2` changed, `u` unmerged, `?` untracked).
- `git for-each-ref --format='%(refname) %(objectname) %(worktreepath) %(upstream:track)'`;
  `%(ahead-behind:<ref>)` exists since git 2.41.
- Graph: build lanes yourself from `git log --all --topo-order -z --format=%H%x00%P%x00%D%x00%an%x00%at%x00%s`
  (cap the count, e.g. 1000, lazily load more). lazygit's `pkg/gui/presentation/graph` is a good reference
  for the pipe/lane algorithm.
- Commit diff: `git show --stat -p <sha>`. Worktree diff: `git -C <wt> diff HEAD` plus
  `git -C <wt> ls-files --others --exclude-standard`.

## Refresh strategy

Editing a file does NOT touch `.git`, so watching `.git` alone misses new dirty state. Combine:
1. fs watch (notify crate) on the common git dir (`git rev-parse --git-common-dir`): `refs/`,
   `packed-refs`, `HEAD`, and each linked worktree's admin dir `<common>/worktrees/<name>/` (its `HEAD`,
   `index`). Watch directories, not files (git writes via `*.lock` + rename); ignore `*.lock`; debounce
   ~150ms. On event → reload refs/graph and the affected worktree's status.
2. fs watch on each worktree's directories (one non-recursive watch per dir; skip `.git`, git-ignored
   dirs from `ls-files -o -i --directory`, nested repos; new dirs get watched as they appear). An edit
   runs `git status` for that worktree only, at most once per 2s (`MIN_GAP`). Set up before the first
   status pass so no edit slips in between.
3. Poll as a safety net, spread out: each worktree has its own slot, run one at a time. Unwatched
   worktrees every `--interval` (60s), watched ones every 10 min (`FALLBACK`; network filesystems only
   report this machine's changes). `--no-poll` disables polling (events + `r` only).
4. Pure polling where events can't work: the filesystem type comes from `/proc/self/mountinfo`
   (`fsinfo.rs`); 9p/drvfs (WSL2 `/mnt/c`), virtiofs and FUSE aren't watched at all. Worktrees over
   10k dirs or past the inotify limit are polled too. The dev machine is WSL2.
One shared cache per common dir; don't redo graph work when refs haven't changed (fingerprint refs).
UI thread only renders snapshots received over a channel.

## herdr agent activity

Goal: see at a glance whether an agent is working in each worktree, and above all whether one is
**blocked waiting for the user**. Works when canopy runs standalone in any terminal, not only inside
herdr; when herdr isn't reachable the feature is invisible. `--no-herdr` turns it off.

- **Transport:** talk to the herdr server directly over its Unix socket with newline-delimited JSON
  (one request per line, responses echo `id`). No shelling out to the `herdr` binary. Socket path:
  `HERDR_SOCKET_PATH`, else `HERDR_SESSION=<name>` → `~/.config/herdr/sessions/<name>/herdr.sock`, else
  `~/.config/herdr/herdr.sock`. No socket / connection refused → hidden, retried with backoff.
- **Reads (protocol 22 / herdr 0.9.3, shapes from `herdr api schema --json`):** `agent.list` →
  `result.agents[]` with `pane_id`, `workspace_id`, `agent`, `name`, `agent_status`
  (`idle|working|blocked|done|unknown`), `cwd`, `foreground_cwd`; `workspace.list` →
  `result.workspaces[]` with optional `worktree.checkout_path`.
- **Mapping agent → worktree:** longest path-prefix match of the agent's `foreground_cwd` (else `cwd`),
  canonicalized, against the canonical worktree paths; if nothing matches, its workspace's
  `checkout_path`. Agents outside this repo are ignored.
- **Live updates:** a second connection does `events.subscribe` for pane/workspace/worktree lifecycle
  events plus `pane.agent_status_changed` per agent pane (that subscription requires a `pane_id`).
  Per herdr's docs: subscribe first, then do authoritative reads; events are only invalidation signals
  that trigger another `agent.list` + `workspace.list` (bursts are coalesced). When the set of agent
  panes changes, resubscribe. On `events_lost` (error on the subscription, which herdr then closes),
  EOF or a broken socket: reconnect with backoff. If subscribing is rejected twice, poll `agent.list`
  every 2s instead.
- **Display:** a marker per worktree by its most urgent agent state — `◆ blocked` (bold red),
  `▶ working`, `✓ done`, `· idle` — with `×N` when several agents share a worktree; the same glyph on
  the worktree's `[name]` label in the graph; footer `herdr: 3 agents, 1 blocked`.
- **The only write-like call:** `a` on a worktree sends `agent.focus` for its (most urgent) agent so
  herdr jumps to that pane. canopy never prompts, sends keys to, starts or stops agents.
- **JSON:** a small hand-written parser (`herdr/json.rs`) instead of serde, to keep dependencies lean.
- **Learned from the live server:** herdr answers one request per connection and then closes it, so
  every request opens a fresh connection; only the subscription connection stays open. Tests run the
  client against a fake server on a temporary socket (events, `events_lost`, focus, unreachable); the
  live check against the real server used only `agent.list`, `workspace.list` and `events.subscribe`
  (`cargo run --example herdr`).

## Milestones

1. Scaffold: cargo project, module layout (git layer / model / ui / watch), CI-free `cargo test` setup,
   a `scripts/mkfixture.sh` that builds a throwaway repo with several worktrees in various states
   (clean, dirty, ahead, detached, untracked-only) under a temp dir for manual and automated testing.
2. Git layer + model: parse worktree list, status v2, log; unit tests on captured outputs.
3. Worktree panel + live refresh (poll first, then fs watch).
4. Graph with lane rendering, worktree labels, dirty pseudo-rows.
5. Diff view for commits and worktrees.
6. Polish: mouse, resize, help popup (`?`), `canopy [path]` arg, `--interval`, graceful non-repo error.
7. (Later, not now) herdr plugin: `herdr-plugin.toml` with a popup pane running canopy and a startup
   poller that sets a per-workspace `$wt_dirty` sidebar token via `herdr workspace report-metadata`.

## Conventions

- Commit after each milestone (small, descriptive messages). No remote is configured; don't push.
- Keep `cargo clippy` clean and `cargo test` green.
- README.md for users; this file for design intent — update it when design decisions change.

## Design decisions (made during implementation)

- **Graph sources:** `git log --branches --remotes --tags <worktree HEADs>` instead of `--all`, so stash
  and notes refs don't add noise; detached worktree HEADs are passed explicitly. Labels come from
  `for-each-ref` (not `%D`), so they don't depend on which worktree canopy runs in.
- **One row per commit:** lanes are drawn lazygit-style (`│ ╯ ╮ ┴ ┤ …`, two cells per lane) with no
  connector-only rows — denser overview than `git log --graph`. Lane colors are per column.
- **Crossings:** where a merge/branch pipe passes through an unrelated lane, the pipe stays continuous
  (`─`, in the pipe's color) and the lane is broken for that one cell, instead of an ambiguous `┼`.
  Nerd Fonts' branch-drawing range (U+F5D0–U+F60D, from kitty) has arcs and connected nodes but no
  bridge/hop glyph, so plain box drawing is used and works in any font.
- **Trunk lane:** lane 0 is reserved for the base branch tip from the top of the graph (invisible until
  the tip row), so the base branch is a straight line on the left and agent branches stack to the right.
  A dirty base worktree's `◌` row sits on that lane directly above the tip.
- **Uncommitted rows** are synthetic commits whose only parent is the worktree HEAD, inserted right above
  it, so the normal lane logic connects them (they get their own lane when the HEAD has children).
- **Worktree name** = branch name, or the directory name when detached. In the graph a branch that is
  checked out in a worktree is shown only as `[name]`; other refs as `(branch, remote, tag: x)`.
  A dirty worktree's `[name]` is on its `◌` row only (not repeated on its HEAD). Prunable worktrees
  don't get `[name]` labels.
- **Colors follow lanes:** each lane gets a color when it is allocated (by column), except that an
  uncommitted row's lane inherits the color of the lane it joins, so a worktree keeps its color whether
  dirty or clean. `[name]`, branch and remote labels use their row's lane color (`Row::color`), tags
  stay yellow, and the worktree list colors each name like its graph row.
- **Load on network filesystems (HPC):** each poll is ~1 metadata op per tracked file per worktree,
  which matters for the metadata server, hence worktree watching + 10-min fallback + staggering +
  `--no-poll`. Notify's inotify mask includes OPEN, so our own `git status` produces access events:
  the handler drops all access events (otherwise an endless status loop). Verified with strace: 30
  writes into an ignored `target/` → 0 statuses; 25 edits over 5 s → 4 statuses; `-i 8` with 8
  worktrees → one status per second, evenly spread.
- **Poll interval 60s by default** (user's call; `r` refreshes on demand, `-i` overrides). Measured
  cost of one `git status --no-optional-locks`: ~1 stat per tracked file + a readdir per directory,
  no content reads (e.g. 755 file syscalls for a 500-file tree, ~5 ms). Caveat: canopy never
  writes the index, so "racily clean" entries (files written in the same second as the index, e.g.
  right after `worktree add`) make every status re-read those files' contents until some normal
  git command refreshes the index. fs events still give ~150 ms latency for anything touching `.git`;
  only pure working-tree edits wait for the poll.
- **Remote labels are red** (`C_REMOTE`, not a lane color); local branches and `[name]` use lane colors.
  Gray was tried and dropped: next to the dim age and parentheses the remotes were easy to miss.
  Red is therefore not a lane color (the sixth lane is orange).
- **Pane chrome:** each pane has only a horizontal rule on top carrying its title (cyan when focused),
  plus a plain rule above the footer; no side lines, those columns stay blank (`render::panel`, with
  `render::content` for hit-testing/paging). Inside tmux/herdr, which draw their own pane borders,
  boxes were visual clutter. Only the `?` popup keeps a full frame, since it floats over content.
- **Selection highlight** is the same strength in both panels whatever has focus (focus is shown by
  the border color).
- **Ahead/behind:** `↑a ↓b` vs the base branch (`--base`, else first existing of main/master/trunk/
  develop, else the main worktree's branch), computed with `rev-list --left-right --count` and cached
  per (base sha, head sha). `⇡a ⇣b` vs upstream from status. The base worktree shows upstream only.
- **Refresh:** every poll tick re-runs `worktree list` + `for-each-ref` (cheap) and all statuses; the
  refs fingerprint gates the log/graph reload, so polling alone covers repos where fs events don't fire.
  fs events only reduce latency. Startup publishes three snapshots: worktree list, + statuses, + graph.
- **Repo-wide git commands run with `-C <common dir>`**, which works for bare and non-bare repos alike.
- **Worktree diff uses `git diff-index -p --stat -M HEAD`, not `git diff HEAD`.** Verified on git 2.43:
  porcelain `git diff` refreshes and rewrites the index (taking `index.lock`) when it sees stat-dirty
  files, even with `--no-optional-locks`. The plumbing never writes. Untracked files are listed via
  `ls-files --others --exclude-standard` and read from disk directly (first 50, 256 KiB / 500 lines each).
  Unborn HEAD diffs against the empty tree (`hash-object -t tree --stdin`, nothing written).
- **Diff view is per file:** `diff::load` returns a `DiffDoc` (overview = everything before the first
  `diff --git`, i.e. commit header/message/stat or the worktree stat + untracked list; then one
  `FileDiff` per file with path, status letter, +/- counts). Untracked files are appended as `?` entries.
  The view (`ui/diff_view.rs`) has a file-list pane and a body pane; it opens on the first file.
- **Enter on a clean worktree** shows its HEAD commit (there's no worktree diff to show).
- **Layout:** stacked by default (graph on top, worktrees below, both full width), so worktree rows can
  show last-edit time, HEAD commit and path, and the graph gets room for long subjects and an author
  column (≥ 100 cols). `v` / `--layout side` switches to side-by-side; narrow terminals (< 72 cols)
  always stack. The worktree panel takes the height it needs, up to half the screen. Paths are shown
  relative to the worktrees' common parent directory.
- **Insets (expand in place), graph only:** the graph is a flat list of visual lines (`ui/inset.rs`):
  base rows plus, for open rows, the subject's wrapped continuation lines (starting at the message
  body's column; the subject is not truncated while open), the message body wrapped to the panel width, one line per
  file, and for open files their diff
  from the first hunk (max 200 lines, then "… n more"). The cursor moves over every visual line
  (magit-style). Space/click toggles a row or file; Space/← inside a diff folds the enclosing section;
  Enter/double-click opens the full view at that file/line (a double-click undoes its first click's
  toggle); Esc folds the row under the cursor (or everything). Wrapping is recomputed when the graph
  width changes (`App::set_wrap_width`, called from render). The worktree panel doesn't expand
  (decided after trying it: the graph's `◌` rows cover it with more room); instead Space/→/←/Esc
  in the worktree list move the graph cursor to that worktree's row and act there. Expansion is keyed by row
  identity (commit sha / worktree path); docs are
  cached per target (commit docs forever, capped; worktree docs keyed by status + last-edit stamp,
  reloaded while shown, dropped otherwise). Graph insets keep drawing the lanes passing by
  (`Row::below`). The cursor is relocated by line identity when refreshes change lines above it.
- **Last edited** = newest mtime among the paths `git status` reports as changed (first 256), so an
  agent still writing to an already-modified file shows up, and open worktree diffs reload on it.
- **Worktree rows show HEAD sha + age, not the subject** (the graph has it). HEAD commit times come
  from one `git log --no-walk=unsorted` over the HEAD shas, cached by sha.
- **Arg parsing is hand-rolled** (no clap) to keep dependencies to ratatui, notify, anyhow, unicode-width.
  ratatui is used with default features off (`crossterm`, `layout-cache`) and its crossterm re-export.
- **Static binary:** the default glibc build links libc dynamically; `--target x86_64-unknown-linux-musl`
  gives a fully static ~1.2 MB binary (no C deps, works out of the box with rustup's musl target).
- **Perf baseline** (`scripts/mkbigrepo.sh`, 20k commits, 16 worktrees, no commit-graph, WSL2): worktree
  list ~25 ms, all statuses ~35 ms, graph ~100 ms after start (`cargo run --release --example startup`).
- **Testing:** integration tests build a fresh fixture under `std::env::temp_dir()` per test
  (`tests/common`), never inside the repo. The TUI was exercised by driving it in a private tmux server
  (`tmux -L <name>`), including injected SGR mouse sequences.
