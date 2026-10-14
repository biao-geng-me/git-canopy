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

This is not another general git client. No staging, committing, rebasing, etc. Read-only monitoring,
with one exception the user confirms each time: cleaning up a worktree whose work is already merged.

## Requirements

- **Self-contained:** single static binary; the only runtime dependency is the `git` executable.
  Shell out to git — no libgit2/gix. (Matches whatever the user's git sees, config included.)
- **Fast:** startup feels instant on repos with thousands of commits and ~10–20 worktrees; refresh never
  blocks input.
- **Overview first, details on demand:** the main screen answers "what is every tree doing, and which are
  dirty?" Drill-down (diff, full commit message) via keyboard and mouse.
- **Never interfere with agents:** every git invocation uses `--no-optional-locks` (or
  `GIT_OPTIONAL_LOCKS=0`) so `status` never grabs `index.lock` while an agent commits. Never write to the repo,
  except the confirmed cleanup of a worktree verified safe (see Design decisions, Cleanup).

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
- **The only write-like calls**, both on the user's explicit request: `a` (or a double-click on the
  agent marker) sends `agent.focus` for the worktree's most urgent agent so herdr jumps to that pane;
  `s` (or a double-click on the path) opens a shell pane below canopy's via `pane.split` (see Shells);
  `t` does the same and then types ` exec <git tui>` into that new pane (`pane.send_input` with
  `keys: ["Enter"]`, the exact request `herdr pane run` sends, captured with a logging fake socket),
  since `pane.split` takes no command; Backspace cleanup sends `worktree.remove` for the workspace
  backed by the removed checkout (see Cleanup). canopy never prompts, sends keys to, starts or stops
  agents; the only input it sends goes to a pane it just created.
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

## Roadmap after 0.2

Still read-only apart from cleanup. No git operations on work in progress (staging, committing,
rebasing in an agent's worktree races the agent and breaks the "never interfere" guarantee); a key
hands off to a real git TUI instead. Cleanup is different: it only touches a worktree whose work is
in base, with no busy agent, after the user confirms.

- **0.3, reviewing agent work (built):** `t` opens a git TUI (lazygit, …) in the worktree, the same way `s`
  opens a shell; a change-size column (`+412 −38` vs the base branch, uncommitted included); `b` diffs
  a worktree against its merge base with the base branch (everything the agent did); overlap warnings
  between worktrees that touch the same files.
- **Cleanup (built):** `⌫` marks worktrees that are safe to clean up; Backspace removes one (checkout,
  branch, herdr workspace) after a confirm popup. canopy's one write, see Design decisions. Maybe
  later: Backspace on a merged branch label without a worktree deletes just the branch.
- **Later:** graph search and a per-worktree filter; `e` opens `$EDITOR` at the file/line under the
  cursor.
- Not planned: notifications (herdr already notifies on blocked agents).

### Multi-repo (0.4; steps 1–3 built)

Agents often work in several projects at once; one canopy should be able to watch all of them.

Built (step 1): every `PATH` argument is a repository (deduplicated by common dir in `main.rs`), each with its own `watch::Worker`; `App::repos` holds per-repo state (`Repo`: snapshot, worker, herdr agent mapping, the parked graph `Panel`, load-more bookkeeping), and `App::snap`/`App::graph` are those of `App::active`. **Invariant: the active repo is the repo of the selected worktree-list row** (`App::follow_wt_selection`, run after every handled message and on moves/clicks), so everything that acts on "the focused worktree" or the graph keeps using `self.snap` and plain worktree indexes. The worktree list's base rows are `App::wt_rows` (`WtRef::Header(r)` / `WtRef::Tree(r, wi)`); only its rendering, per-row markers (`App::cleanup_in`, `agent_activity_in`), the herdr footer and refresh summary span all repos. Header rows (`Key::Repo(common dir)`, never expand) only with several repos; duplicate repo names get their parent directory (`App::repo_label`). Built (step 2, shared resources): one `watch::Hub` (`watch/hub.rs`) holds the only notify watcher; each worker registers (`Hub::register` → `Watches`, dropped = all its watches gone) and the hub routes each event to the owner of the nearest watched directory at or above its path, so a repo nested in another's checkout doesn't wake the outer one; watches are refcounted per directory; `Watches::remaining` is a global budget (`MAX_TOTAL_DIRS` 50k, at most half of `fs.inotify.max_user_watches`) on top of the per-tree `MAX_WATCH_DIRS`, and a tree that doesn't fit is polled. `watch::spawn` still makes its own hub (tests); the UI uses `spawn_with` with one hub. Every `par_map` job takes one of `GLOBAL_PARALLELISM` (8) process-wide slots (`SLOTS`), so N repos at startup don't run N×6 git processes; UI-side diff loads don't take slots (they stay responsive). Built (step 3, `--herdr-repos`): repositories are added and removed at runtime, so each `Repo` has a stable `id` (snapshots arrive as `UiMsg::Snapshot(id, …)`, never by index) and its own `common_dir`; `App::add_repo` / `remove_repo` keep the selection on its row and the active-repo invariant. Discovery (`App::discover_repos`, on every herdr view): `herdr::dirs` (agent cwds + workspace checkouts) not seen before are resolved with `git::discover` off the UI thread (`UiMsg::Discovered`, cached per directory forever, misses included), then `reconcile_repos` adds every repository named and gives each herdr-found one that no longer is a grace (`HERDR_REPO_GRACE`, 2 min: an agent restarted, or the user reviewing its work) before removing it; the event loop wakes for that deadline (`App::next_deadline` / `tick`). Command-line repos are never removed; herdr unreachable removes nothing. Dropping a `watch::Handle` now sends `Msg::Stop` (the hub holds a sender, so the channel alone never closed and a dropped worker kept running). Still to do: the `(host, path)` keys + Worker request API from the remote prep bullet.

- **Boundary:** everything per repository stays per repository. One `watch::Worker` (and one
  `Snapshot`) per common dir, unchanged; `App` holds a list of repos instead of one `snap` + `worker`.
  **Rule until then:** new features compute their data in the worker into `Snapshot`/`WorktreeView`
  and never assume the UI has only one repo (key things by worktree path, which is unique across repos,
  or by common dir, never by index into one global list).
- **Prepare for remote machines (0.5) in 0.4:** key worktrees and repos by `(host, path)` / `(host, common dir)`, with host = local for now (a path is not unique across machines: `/home/me/proj` exists on the laptop and the server). And the UI stops running git itself: diffs, copy (`y`), overlap docs and cleanup become requests to the Worker that owns the repo (a message API, local-only at first), which is the seam a remote Worker plugs into.
- **Layout:** the worktree panel lists all repos, grouped under a header row per repo (repo name, base
  branch, worktree/dirty counts); the graph shows the repo of the selected worktree (or header). Moving
  the selection into another repo swaps the graph; per-repo graph cursor/expansion state is kept. Tabs
  and stacked graphs were considered: tabs are no better than two canopies in herdr panes, and several
  graphs on one screen get unreadable.
- **Which repos:** every `PATH` argument (`canopy ~/a ~/b`); with `--herdr-repos`, also every repo that
  a herdr agent's cwd or a workspace's `checkout_path` is in (discovered with `git rev-parse
  --git-common-dir`, deduplicated by common dir, added/removed as agents come and go). A single
  repo behaves exactly as today (no header row).
- **Shared resources (built):** one herdr connection for all repos (agents are matched against every repo's worktree paths); a global cap on concurrent git processes across workers; one inotify watcher shared by all workers with a global budget; `--base` applies to every repo, else per-repo detection.
- **Footer/details:** herdr summary across all repos; the details line names the repo when there are
  several.

### Remote machines (planned for 0.5, not built)

herdr does cross-machine development (saved SSH machines, each with its own herdr server; people pair it with a personal network such as Tailscale). canopy should show every tree in one view, even when the trees are in different forests (clones on different machines). Found on 2026-10-12: an agent orchestrating sub-agents in a herdr on an HPC compute node (gh-b-001) while canopy ran on the login node; the trees were on the shared filesystem, but canopy saw no agents in them (they live in gh-b-001's herdr, whose socket is node-local) and their edits came only with the 10-min fallback (inotify sees only this machine's changes).

- **Shape: `canopy serve` over ssh stdio.** The local canopy runs `ssh <host> canopy serve <repos>`; the remote side runs the normal `watch::Worker`s there and streams snapshots back over stdin/stdout, and answers requests (diffs, copy text, cleanup plan/run). No listening ports: auth, encryption and NAT are ssh's job (Tailscale SSH covers the personal-network case); herdr chose the same model. Not inside herdr panes: canopy needs no terminal there.
- **What that gives for free:** the remote serve talks to *its own machine's* herdr over the local socket (agent markers per machine with the existing client, no dependency on herdr's undocumented `remote-api-bridge`); git and inotify run where the edits happen (~150 ms latency instead of the fallback poll); cleanup is planned, re-verified and run on the owning host.
- **Two separate concerns:** where git runs (one Worker per (host, common dir)) and where agents live (herdr servers). With separate clones they coincide. On a shared filesystem they don't: one repo, agents from several herdr servers, so a serve must be able to contribute *only agents* for a repo another host owns, and running two Workers on the same repo (double git load, trees shown twice) must be avoided. That needs a repo identity across hosts: canonical common-dir path plus a probe (same refs fingerprint, common dir inode), with a config override where that's unsure. Prefer watching from the host where the agent works (its inotify sees the edits).
- **Layout:** the repo header row names the host (`three-ux @ gh-b-001`); clones of one origin on several hosts are separate forests grouped together, each with its own graph (merging graphs across clones would need their objects in one place).
- **Practicalities:** a static musl binary per arch on the remote (installed by the user, or `canopy remote install <host>` copies it, like herdr's `machine add`); a version handshake as the first message; hand-written serialization (no serde) of `Snapshot`, requests and capped/paged diffs; `BatchMode=yes` ssh, never prompting, an unreachable host hidden with backoff like an unreachable herdr. Discovery: `canopy host:path` arguments, optionally herdr's saved machines (`herdr machine list`), and per host the repos its herdr's agents are in (`--herdr-repos`).
- **Rejected:** reading other machines' agents through herdr's `remote-api-bridge` (`ssh <host> herdr --session default remote-api-bridge` pipes the normal JSON API, events included; verified with herdr 0.9.3): undocumented, and it fixes only agent markers, not the edit latency.

## Conventions

- Commit after each milestone (small, descriptive messages). Remote: `origin` =
  github.com/biao-geng-me/git-canopy (public). Push and publish releases only when the user asks.
- Releases: bump `Cargo.toml`, commit "Release x.y.z", annotated tag `vx.y.z` ("canopy x.y.z"), then a
  GitHub release with `canopy-x86_64-unknown-linux-musl.tar.gz` (static musl build: `canopy`,
  README, LICENSE) and its `.sha256` (`scripts/release.sh`). The asset name has no version so
  `releases/latest/download/...` in the README stays valid.
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
- **Pane chrome:** each pane has only a horizontal rule on top carrying its title after a short `──`
  lead-in (cyan when focused), plus a plain rule above the footer; no side lines, those columns stay
  blank (`render::panel`, with `render::content` for hit-testing/paging). Inside tmux/herdr, which draw their own pane borders,
  boxes were visual clutter. Only the `?` popup keeps a full frame, since it floats over content.
- **Shells:** `s` / double-click on a worktree's path opens a shell there, first that works:
  (1) a herdr pane split below canopy's own (`pane.split` with `target_pane_id` = `HERDR_PANE_ID`,
  `direction: "down"`, the worktree as `cwd`, `focus: true`); herdr runs the user's shell there and
  closes the pane when it exits. Only when canopy runs inside herdr, never with `--no-herdr`.
  A popup came first (herdr 0.9.3 only pops up plugin panes, so it needed a linked plugin manifest),
  but popups can't be resized or moved, so it was dropped for a plain split, which needs no plugin;
  (2) the same inside tmux: `tmux split-window -v -c <path> -t $TMUX_PANE` (a `display-popup` came
  first and was dropped for the same reason); (3) an inset
  terminal in the lower half of the graph's space (`ui/term.rs`): a pty from `openpty` (libc) with
  `$SHELL` as session leader, emulated by the `vt100` crate (which doesn't answer queries, so cursor
  position / device attribute requests are answered from its callbacks), drawn cell by cell. While
  it has focus every key goes to it except `Ctrl-]`; `Tab` cycles worktrees → graph → shell. One inset
  shell at a time (a second request focuses it rather than hanging it up); closing canopy sends the
  shell's session SIGHUP. These two crates are the price of the inset; canopy is Unix-only anyway.
  Tested live: the inset in a private tmux, the herdr split in a fully separate herdr instance
  (relative `HOME` under the scratchpad), agent focus and the split request against a fake socket
  server.
- **Copying (`y` chords):** `y` then a key (`y` obvious thing, `h`/`s` sha, `b` branch, `m`/`M`
  subject/message, `p` path, `f` file, `d` diff) copies from the line under the cursor or the open
  diff (`ui/copy.rs`, `CopyCtx`); the footer lists what's available while `y` is pending. Text is
  produced and handed over off the UI thread (message via `git log -1 --format=%B`, diffs via
  `diff::patch`, which returns raw `git apply`-able text — the view's lines are sanitized, tabs
  expanded — with untracked files as new-file sections). Delivery (`clipboard.rs`, no deps), first
  that works: a local program (`clip.exe` fed UTF-16LE *without* BOM — verified: with one the BOM
  ends up in the clipboard; `pbcopy`, `wl-copy`, `xclip`, `xsel`; skipped over SSH), `tmux
  load-buffer -w -`, else OSC 52, which only the UI thread writes (between frames). Tested live:
  tmux buffer, OSC 52 caught by a private tmux with `set-clipboard on`, and `clip.exe` read back
  with PowerShell's `Get-Clipboard`.
- **Worktree row fields are clickable:** render records each row's agent-marker and path column
  ranges (`App::wt_fields`); a double-click on them focuses the agent / opens a shell, anywhere else
  opens the diff as before.
- **Mouse:** the line under the pointer gets a faint highlight (`HOVER`, fainter than the
  selection; `App::hover` is a screen row, so it follows scrolling, and keys clear it). Motion
  events only redraw when the hovered row changes. Clicking a worktree always jumps the graph to it,
  even if it was already selected (the graph cursor may have been moved away); a second click on it
  toggles its graph row, like Space in the list. The diff view's file list has the same hover
  (`DiffView::hover`); its click already only selects, there is nothing to toggle.
- **Selection highlight** is the same strength in both panels whatever has focus (focus is shown by
  the border color).
- **Ahead/behind:** `↑a ↓b` vs the base branch (`--base`, else first existing of main/master/trunk/
  develop, else the main worktree's branch), computed with `rev-list --left-right --count` and cached
  per (base sha, head sha). `⇡a ⇣b` vs upstream from status. The base worktree shows upstream only.
- **Change size** (`+412 -38`, wide rows): per worktree, merge base with the base branch → working tree,
  tracked files only. Computed in the worker, not with one `diff-index <merge base>` (that stats every
  file, as costly as a status) but as `diff-tree --numstat` merge base → HEAD (cached forever by the
  pair) plus `diff-index --numstat <merge base> -- <paths status reported>` (stats only those; all
  paths when status listed `MAX_PATHS`), merged per path (`Changes::merge`). `--no-renames`
  everywhere so every entry has one path (a rename counts as delete + add). `diff-index` doesn't
  refresh the index, so stat-dirty-but-unchanged files come back 0/0 and are dropped.
- **Overlaps:** for every pair of sides (worktrees with a status, plus the base branch itself when no
  worktree has it checked out), the files both changed in *their own work*, committed or not. A side's
  own work starts at the later of the pair's merge base and its fork from base (`merge-base` of the
  two decides which is later): without that, every branch that merely contains base's newer commits
  "overlapped" (caught by the fixture test), and a branch built on another overlapped all of its
  files. The base side's work starts at the pair's merge base (what base changed since the other
  forked = rebase/merge conflict risk). Uncommitted files are status's paths (untracked dirs as
  `dir/`, matching everything under them, `model::shared_files`). Merge bases and commit diffs are
  cached; O(n²) merge-base calls at startup (190 for 20 worktrees, parallel, after the graph is up),
  then only for moved HEADs. Shown on the row (`⚠ name +n`), in the details footer (files), and in
  the diff file list (`⚠ also in name`). `o` / a double-click on the `⚠` field opens
  `diff::Target::Overlaps`: one entry per shared file, its file list drawn as a table
  (`DiffDoc::columns` = this worktree then each other side, `FileDiff::cells` = each side's
  `+a -d`, `·` when untouched; the path column takes what the side columns leave, min 16, cut from
  the left since the diff pane's title has the full path), and every side's diff of the file
  under `── name, since <sha> ──` headings, this worktree first, each from its own-work start (kept
  in `model::Overlap` as `from`/`other_from`/`other_head`) to its working tree (`diff-index`, plus
  untracked files), or `diff-tree` to the base tip for a base side without a worktree. Two
  `diff-index` calls per pair, not per file. The view reloads when this worktree, any other side
  in it, or the overlaps change. `yd` gives both sides' raw patches with `#` comment lines: for
  pasting to an agent, not for `git apply`.
- **Merged / safe to clean up:** `WorktreeView::landed`, computed in the worker per (base sha, HEAD,
  branch) and cached. HEAD in base (`ahead == 0`): `Merged` if the branch's reflog has more than its
  creation entry (fast-forwarded or merged after commits of its own), else `Unused` (a fresh
  branch, a detached checkout of a base commit, or no reflog). `Unused` is removable too (user's
  call: no commits to lose, e.g. read-only test checkouts) but must not read as "done": gray `⌫`,
  no `merged` label, details "nothing to lose". HEAD not in base:
  `Applied` when every file it changed since the fork is identical at the base tip (`diff-tree
  --quiet <head> <base> -- <paths>`, read-only, unlike `merge-tree --write-tree`), which catches
  squash merges, rebases and cherry-picks; conservative when base has changed those files again.
  Merged worktrees show `merged` instead of `↑a ↓b`, no change size, and take no part in overlaps
  (their work being in base isn't a conflict). `WorktreeView::cleanup` adds the rest: not the main
  worktree, not on base, not locked or mid-operation, no staged/modified/unmerged files, no agent
  other than idle/done (`App::cleanup`), and the two guards below → `⌫` green (`Safe`), yellow when only untracked files would
  be lost (`Untracked`). Same glyph after `[name]` in the graph; the details line spells it out.
  `⌫`, not `✓`, which is herdr's "done". The `◌` row's "uncommitted" word is red.
- **Cleanup guards against agents canopy can't see** (an agent in another herdr, e.g. on another machine sharing the filesystem, or one that hasn't changed anything yet). (1) `Unused` worktrees that git used in the last hour (`model::FRESH_SECS`; `WorktreeView::git_used_at` = newest mtime of the admin dir's `HEAD`, `index`, `commondir`, `git::worktree::last_used`) aren't offered: a fresh tree is indistinguishable from a just-started agent's, and holding back something with nothing to lose costs only waiting; merged work isn't held back. (2) A checkout in herdr's worktree directory (`[worktrees] directory` in herdr's `config.toml`, default `~/.herdr/worktrees`, `herdr::worktree_dir`) is only cleaned up through a workspace the reachable herdr has for it (`App::herdr_unseen`, `model::HERDR_UNSEEN`); herdr unreachable or `--no-herdr` → refused too. Both are checked again in `cleanup::plan`/`run`. Tests backdate the admin dir (`Fixture::age`).
- **Cleanup (Backspace, `cleanup.rs`):** on a `⌫` worktree (its row, or its `[name]`/`◌` graph row;
  on a commit several worktrees share, the cleanable one). Anything else gets a footer reason
  (`WorktreeView::cleanup_blocker`, the same function behind `⌫`). `cleanup::plan` runs off the UI
  thread: re-verifies (fresh `status`: no changes, *no untracked* files; same HEAD and branch;
  `cleanup::landed` recomputed against the base tip), reads herdr fresh (busy agent in the worktree →
  refuse; the workspace backed by the checkout is canopy's own `HERDR_WORKSPACE_ID` → refuse), and
  lists ignored files (`ls-files -o -i --directory`), which `git worktree remove` deletes silently.
  The popup shows exactly that; `y`/Enter/Backspace runs `cleanup::run` (so a double Backspace is a
  quick delete; a second Backspace while still checking, on the same worktree, queues the confirm
  and skips the popup, `ui::Cleaning`), which verifies everything again, then
  removes the checkout: herdr's `worktree.remove {workspace_id, force:false}` when a workspace is
  backed by it, else `git worktree remove` (never `--force`); then `git branch -D` only if the branch
  still points at the verified sha (`-D` because squash-merged branches fail `-d`; safe because of
  our own check). Footer: the sha and `git branch <b> <sha>` to restore. Untracked files are refused
  rather than forced (user can delete them, `s` opens a shell). Remotes never touched. Agents
  outside that workspace with a cwd inside it (idle/done) are listed as left in a deleted directory;
  unrelated workspaces are never closed. **herdr 0.9.3 `worktree.remove`, verified on a throwaway
  server** (relative `HOME`, every `HERDR_*` unset): runs `git worktree remove`, so a dirty checkout
  fails with `dirty_worktree_requires_force`; a clean one is removed (ignored files included) and
  the workspace is closed with all its panes; **the branch is left**, hence canopy deletes it; works
  the same for worktrees made by herdr (`worktree create`) or by git and opened in herdr. The main
  checkout's workspace also has a `worktree` record (`is_linked_worktree: false`). Tests: plain git,
  squash-merged, refusals after planning, and a fake herdr socket (`tests/cleanup.rs`); live: the
  popup and both removal paths against the throwaway herdr in a private tmux.
- **Diff vs base (`b`):** `diff::Target::Branch { path, base }` resolves `merge-base <base> HEAD` at
  load time (so it never goes stale) and shows `diff-index -p <merge base>` + untracked, like the
  worktree diff. Copy (`yd`) gives the same as a patch.
- **Git TUI (`t`):** `--git-tui` / `CANOPY_GIT_TUI`, else the first of lazygit, gitui, tig on PATH,
  opened exactly like a shell (herdr split, tmux split with the command, or the inset running
  `$SHELL -c 'exec <cmd>'`), so its pane closes when the TUI exits. `t`, not `g` (taken by top/bottom).
- **Refresh:** every poll tick re-runs `worktree list` + `for-each-ref` (cheap) and all statuses; the
  refs fingerprint gates the log/graph reload, so polling alone covers repos where fs events don't fire.
  fs events only reduce latency. Startup publishes three snapshots: worktree list, + statuses, + graph.
  A worktree whose HEAD or branch moved in a refs reload gets a fresh status right away
  (`Worker::moved`): a commit from another machine sharing the filesystem sends no index event
  here, and the stale (pre-commit, dirty) status would otherwise last until the 10-min fallback,
  hiding `⌫` meanwhile.
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
  (magit-style). Space, or a click on the line already selected in the focused panel, toggles a row
  or file (a first click only focuses/selects); Space/← inside a diff folds the enclosing section;
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
- **Arg parsing is hand-rolled** (no clap) to keep dependencies to ratatui, notify, anyhow, unicode-width
  (plus libc and vt100 for the inset shell).
  ratatui is used with default features off (`crossterm`, `layout-cache`) and its crossterm re-export.
- **Static binary:** the default glibc build links libc dynamically; `--target x86_64-unknown-linux-musl`
  gives a fully static ~1.2 MB binary (no C deps, works out of the box with rustup's musl target).
- **Perf baseline** (`scripts/mkbigrepo.sh`, 20k commits, 16 worktrees, no commit-graph, WSL2): worktree
  list ~25 ms, all statuses ~35 ms, graph ~100 ms after start, change sizes + overlaps (136 pairs)
  ~350 ms after the graph (`cargo run --release --example startup`).
- **Testing caveat:** each watching test makes its own hub, i.e. one inotify instance; on a node where the user's other processes already hold most of `fs.inotify.max_user_instances` (128 on gh-b-001, ~120 in use) parallel watch tests time out waiting for "watched". Run `cargo test -- --test-threads=2` there.
- **Testing:** integration tests build a fresh fixture under `std::env::temp_dir()` per test
  (`tests/common`), never inside the repo. The TUI was exercised by driving it in a private tmux server
  (`tmux -L <name>`), including injected SGR mouse sequences.
