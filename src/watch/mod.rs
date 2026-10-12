//! Background refresh: file system watching + (spread out) polling.
//!
//! One worker thread owns all repository state and produces immutable [`Snapshot`]s for `emit`.
//!
//! - **Git dir events** (refs, HEADs, indexes in the common dir) refresh refs and the affected
//!   worktree's status within ~150 ms.
//! - **Worktree events**: each worktree's directories are watched (minus `.git`, ignored and
//!   nested repos, see [`trees`]); an edit runs `git status` for that worktree only, at most once
//!   per [`MIN_GAP`].
//! - **Polling** catches what events miss: unwatched worktrees every `interval`, watched ones
//!   every [`FALLBACK`] (changes from other machines on network filesystems). Polls are spread
//!   over the interval, one worktree at a time, instead of all at once. `poll: false` disables
//!   polling entirely (manual refresh only).
//!
//! Refs are fingerprinted so the log/graph is only recomputed when something actually moved.

mod trees;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::fsinfo::{self, Events};
use crate::git::{self, RepoPaths, changes::FileChange, log::Commit, refs::RefEntry};
use crate::graph::Graph;
use crate::model::{self, Changes, Landed, Overlap, Snapshot, WorktreeView};
use trees::TreeWatch;

/// Coalesce bursts of fs events (git writes several files per operation).
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Minimum time between edit-triggered statuses of one worktree (a busy agent or build).
pub const MIN_GAP: Duration = Duration::from_secs(2);
/// Poll period for watched worktrees (at least `interval`): catches changes events miss, e.g.
/// edits made from another machine on a network filesystem.
pub const FALLBACK: Duration = Duration::from_secs(600);
/// Worktrees with more directories than this are polled instead of watched.
const MAX_WATCH_DIRS: usize = 10_000;
/// Max concurrent git processes for startup and manual refreshes.
const PARALLELISM: usize = 6;
/// Default for [`Config::page`].
pub const GRAPH_PAGE: usize = 1000;

#[derive(Clone, Debug)]
pub struct Config {
    /// Poll period for worktrees that can't be watched, and for refs.
    pub interval: Duration,
    /// Poll at all; without it, updates come from fs events and manual refreshes only.
    pub poll: bool,
    /// Use fs notifications (git dir and worktrees).
    pub watch: bool,
    /// Branch to compare worktrees against; auto-detected when `None`.
    pub base: Option<String>,
    /// Commits loaded initially and per "load more" step.
    pub page: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval: Duration::from_secs(60),
            poll: true,
            watch: true,
            base: None,
            page: GRAPH_PAGE,
        }
    }
}

/// Requests from the UI.
#[derive(Debug)]
pub enum Request {
    Refresh,
    LoadMore,
}

enum Msg {
    Req(Request),
    Fs(Vec<PathBuf>),
}

/// Handle to the worker thread; dropping it stops the worker.
pub struct Handle {
    tx: Sender<Msg>,
}

impl Handle {
    pub fn request(&self, req: Request) {
        let _ = self.tx.send(Msg::Req(req));
    }
}

/// Start the worker. `emit` is called from the worker thread with every new snapshot.
pub fn spawn<F>(paths: RepoPaths, cfg: Config, emit: F) -> Handle
where
    F: Fn(Arc<Snapshot>) + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let fs_tx = tx.clone();
    thread::Builder::new()
        .name("canopy-refresh".into())
        .spawn(move || {
            let mut w = Worker::new(paths, cfg, Box::new(emit));
            if w.cfg.watch {
                w.start_watcher(fs_tx);
            }
            w.initial_load();
            w.run(rx);
        })
        .expect("spawn refresh thread");
    Handle { tx }
}

struct Worker {
    paths: RepoPaths,
    cfg: Config,
    emit: Box<dyn Fn(Arc<Snapshot>) + Send>,
    watcher: Option<RecommendedWatcher>,
    watched_worktrees_dir: bool,
    /// Watched worktrees, by path.
    trees: HashMap<PathBuf, TreeWatch>,
    /// Worktrees that couldn't be watched (too big, unreliable fs, watch limit); polled instead.
    unwatchable: HashSet<PathBuf>,

    /// Next poll per worktree.
    due: HashMap<PathBuf, Instant>,
    refs_due: Instant,
    /// Last status run per worktree, for rate-limiting edit-triggered statuses.
    last_status: HashMap<PathBuf, Instant>,
    /// Edit-triggered statuses waiting out [`MIN_GAP`].
    deferred: HashMap<PathBuf, Instant>,

    worktrees: Vec<WorktreeView>,
    refs: Vec<RefEntry>,
    /// Hash of raw `worktree list` + `for-each-ref` output.
    fingerprint: u64,
    base: Option<String>,
    base_sha: Option<String>,
    ab_cache: HashMap<(String, String), (u32, u32)>,
    /// Merge base per (sorted) pair of commits; `None` for unrelated histories.
    mb_cache: HashMap<(String, String), Option<String>>,
    /// Files changed between two commits.
    between_cache: HashMap<(String, String), Arc<Vec<FileChange>>>,
    /// Merge base → working tree counts of each dirty worktree's changed paths, with the key
    /// they were computed for (merge base, status, last edit).
    dirty_cache: HashMap<PathBuf, (u64, Arc<Vec<FileChange>>)>,
    /// Commit time per HEAD sha.
    head_cache: HashMap<String, i64>,
    /// Whether a HEAD's work is in the base branch.
    landed_cache: HashMap<LandedKey, Option<Landed>>,

    limit: usize,
    commits: Arc<Vec<Commit>>,
    commits_key: Option<(u64, usize)>,
    graph: Arc<Graph>,
    graph_key: Option<u64>,

    statuses_loaded: bool,
    error: Option<String>,
}

impl Worker {
    fn new(paths: RepoPaths, cfg: Config, emit: Box<dyn Fn(Arc<Snapshot>) + Send>) -> Self {
        let limit = cfg.page.max(1);
        let refs_due = Instant::now() + cfg.interval;
        Worker {
            paths,
            cfg,
            emit,
            watcher: None,
            watched_worktrees_dir: false,
            trees: HashMap::new(),
            unwatchable: HashSet::new(),
            due: HashMap::new(),
            refs_due,
            last_status: HashMap::new(),
            deferred: HashMap::new(),
            worktrees: Vec::new(),
            refs: Vec::new(),
            fingerprint: 0,
            base: None,
            base_sha: None,
            ab_cache: HashMap::new(),
            mb_cache: HashMap::new(),
            between_cache: HashMap::new(),
            dirty_cache: HashMap::new(),
            head_cache: HashMap::new(),
            landed_cache: HashMap::new(),
            limit,
            commits: Arc::default(),
            commits_key: None,
            graph: Arc::default(),
            graph_key: None,
            statuses_loaded: false,
            error: None,
        }
    }

    /// Repo-wide commands run from the common dir, which works for bare and non-bare repos.
    fn repo_dir(&self) -> &Path {
        &self.paths.common_dir
    }

    fn start_watcher(&mut self, tx: Sender<Msg>) {
        let common = self.paths.common_dir.clone();
        if fsinfo::events_at(&common) == Events::Unreliable {
            return; // e.g. WSL2 /mnt/c: watches "work" but never fire. Poll instead.
        }
        let handler = move |res: notify::Result<notify::Event>| {
            // Access events include our own `git status` opening files: never react to them.
            if let Ok(ev) = res
                && !ev.kind.is_access()
                && !ev.paths.is_empty()
            {
                let _ = tx.send(Msg::Fs(ev.paths));
            }
        };
        let Ok(mut watcher) = notify::recommended_watcher(handler) else {
            return; // Polling still covers everything, just with more latency.
        };
        if watcher.watch(&common, RecursiveMode::NonRecursive).is_err() {
            return;
        }
        let refs = common.join("refs");
        if refs.is_dir() {
            let _ = watcher.watch(&refs, RecursiveMode::Recursive);
        }
        self.watcher = Some(watcher);
        self.ensure_worktrees_watch();
    }

    /// `<common>/worktrees` only exists once a linked worktree was added; watch it when it appears.
    fn ensure_worktrees_watch(&mut self) {
        if self.watched_worktrees_dir {
            return;
        }
        let dir = self.paths.common_dir.join("worktrees");
        if let Some(w) = self.watcher.as_mut()
            && dir.is_dir()
            && w.watch(&dir, RecursiveMode::Recursive).is_ok()
        {
            self.watched_worktrees_dir = true;
        }
    }

    /// Start watching new worktrees' directories and stop watching removed ones.
    fn sync_tree_watches(&mut self) {
        let Some(watcher) = self.watcher.as_mut() else { return };
        let current: HashSet<PathBuf> = self
            .worktrees
            .iter()
            .filter(|w| w.has_worktree())
            .map(|w| w.path.clone())
            .collect();
        self.trees.retain(|path, tw| {
            let keep = current.contains(path);
            if !keep {
                for d in &tw.dirs {
                    let _ = watcher.unwatch(d);
                }
            }
            keep
        });
        let new: Vec<PathBuf> = current
            .into_iter()
            .filter(|p| !self.trees.contains_key(p) && !self.unwatchable.contains(p))
            .collect();
        // Walking the trees is the slow part: do it in parallel, then add the watches.
        let plans = par_map(&new, PARALLELISM, |p| plan_tree(p));
        for (path, plan) in new.into_iter().zip(plans) {
            match plan.and_then(|(ignored, dirs)| watch_dirs(watcher, ignored, dirs)) {
                Some(tw) => {
                    self.trees.insert(path, tw);
                }
                None => {
                    self.unwatchable.insert(path);
                }
            }
        }
    }

    /// Watch a directory that appeared inside a watched worktree (unless it is ignored).
    fn watch_new_dir(&mut self, root: &Path, dir: &Path) {
        let (Some(watcher), Some(tw)) = (self.watcher.as_mut(), self.trees.get_mut(root)) else {
            return;
        };
        let ignored = trees::ignored_dirs(root, dir);
        let budget = MAX_WATCH_DIRS.saturating_sub(tw.dirs.len());
        let dirs = trees::dirs_to_watch(dir, &ignored, budget).unwrap_or_default();
        tw.ignored.extend(ignored);
        for d in dirs {
            if !tw.dirs.contains(&d) && watcher.watch(&d, RecursiveMode::NonRecursive).is_ok() {
                tw.dirs.insert(d);
            }
        }
    }

    /// How often a worktree is polled: watched ones only as a fallback.
    fn period(&self, wt: &Path) -> Duration {
        if self.trees.contains_key(wt) {
            FALLBACK.max(self.cfg.interval)
        } else {
            self.cfg.interval
        }
    }

    /// Give worktrees without a poll slot one, spread evenly over their period.
    fn schedule(&mut self) {
        let live: HashSet<&PathBuf> = self.worktrees.iter().map(|w| &w.path).collect();
        self.due.retain(|p, _| live.contains(p));
        self.deferred.retain(|p, _| live.contains(p));
        let new: Vec<PathBuf> = self
            .worktrees
            .iter()
            .filter(|w| w.has_worktree() && !self.due.contains_key(&w.path))
            .map(|w| w.path.clone())
            .collect();
        let now = Instant::now();
        for (i, p) in new.iter().enumerate() {
            let slot = trees::stagger(now, self.period(p), new.len())[i];
            self.due.insert(p.clone(), slot);
        }
    }

    fn initial_load(&mut self) {
        // Worktree list first (fast), then statuses, then the graph: the overview fills in
        // progressively instead of waiting on the slowest piece. Worktree watches go in before
        // the first statuses, so no edit can slip in between the two unnoticed.
        self.refresh_refs(false);
        self.publish();
        self.sync_tree_watches();
        self.schedule();
        self.refresh_status(None);
        self.statuses_loaded = true;
        self.publish();
        self.refresh_graph();
        self.publish();
        // What each branch changed and where they overlap: needs more git work, so last.
        if self.refresh_changes() {
            self.publish();
        }
    }

    fn run(&mut self, rx: mpsc::Receiver<Msg>) {
        let mut pending: Vec<PathBuf> = Vec::new();
        let mut debounce_until: Option<Instant> = None;
        loop {
            let mut deadline = Instant::now() + Duration::from_secs(86_400);
            let mut consider = |t: Instant| deadline = deadline.min(t);
            if let Some(d) = debounce_until {
                consider(d);
            }
            self.deferred.values().copied().for_each(&mut consider);
            if self.cfg.poll {
                consider(self.refs_due);
                self.due.values().copied().for_each(&mut consider);
            }
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Msg::Fs(paths)) => {
                    pending.extend(paths);
                    debounce_until.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                }
                Ok(Msg::Req(Request::Refresh)) => self.full_refresh(),
                Ok(Msg::Req(Request::LoadMore)) => {
                    if self.commits.len() >= self.limit {
                        self.limit += self.cfg.page.max(1);
                        self.refresh_graph();
                        self.publish();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            if debounce_until.is_some_and(|d| now >= d) {
                debounce_until = None;
                self.on_fs_events(std::mem::take(&mut pending));
            }
            self.run_due(now);
        }
    }

    /// Run whatever is due: deferred edit statuses, the refs poll, worktree polls.
    fn run_due(&mut self, now: Instant) {
        let mut targets: HashSet<PathBuf> = self
            .deferred
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(p, _)| p.clone())
            .collect();
        self.deferred.retain(|p, _| !targets.contains(p));
        let mut changed = false;
        if self.cfg.poll {
            if now >= self.refs_due {
                self.refs_due = now + self.cfg.interval;
                if self.refresh_refs(false) {
                    changed = true;
                    self.after_worktree_list_change(&mut targets);
                }
            }
            targets.extend(self.due.iter().filter(|(_, t)| **t <= now).map(|(p, _)| p.clone()));
        }
        // Polls run one worktree at a time: spreading the load is the point.
        for p in targets {
            let one = HashSet::from([p]);
            changed |= self.refresh_status_with(Some(&one), 1);
        }
        if changed {
            self.refresh_graph();
            self.refresh_changes();
            self.publish();
        }
    }

    /// New worktrees need a watch, a poll slot and a first status.
    fn after_worktree_list_change(&mut self, targets: &mut HashSet<PathBuf>) {
        self.ensure_worktrees_watch();
        self.sync_tree_watches();
        self.schedule();
        targets.extend(
            self.worktrees
                .iter()
                .filter(|w| w.status.is_none() && w.has_worktree())
                .map(|w| w.path.clone()),
        );
    }

    fn full_refresh(&mut self) {
        // A manual refresh also retries worktrees that couldn't be watched before.
        self.unwatchable.clear();
        self.refresh_refs(false);
        self.sync_tree_watches();
        self.due.clear();
        self.schedule();
        self.refresh_status(None);
        self.refresh_graph();
        self.refresh_changes();
        self.publish();
    }

    fn on_fs_events(&mut self, paths: Vec<PathBuf>) {
        let common = self.paths.common_dir.clone();
        let mut targets: HashSet<PathBuf> = HashSet::new();
        let mut edited: HashSet<PathBuf> = HashSet::new();
        let mut new_dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut git_relevant = false;
        for p in &paths {
            let Ok(rel) = p.strip_prefix(&common) else {
                // A working-tree edit in a watched worktree (deepest root wins for nested ones).
                let Some(root) = self
                    .trees
                    .keys()
                    .filter(|r| p.starts_with(r))
                    .max_by_key(|r| r.as_os_str().len())
                else {
                    continue;
                };
                let tw = &self.trees[root];
                if tw.is_noise(root, p) {
                    continue;
                }
                if p.is_dir() && !tw.dirs.contains(p) {
                    new_dirs.push((root.clone(), p.clone()));
                }
                edited.insert(root.clone());
                continue;
            };
            let mut comps = rel.components().map(|c| c.as_os_str().to_string_lossy());
            let first = comps.next().unwrap_or_default();
            if p.extension().is_some_and(|e| e == "lock")
                || matches!(&*first, "objects" | "logs" | "COMMIT_EDITMSG" | "gc.log" | "FETCH_HEAD")
            {
                continue;
            }
            git_relevant = true;
            if first == "worktrees" {
                if let Some(name) = comps.next() {
                    let admin = common.join("worktrees").join(&*name);
                    if comps.next().is_some_and(|c| c == "logs") {
                        continue;
                    }
                    if let Some(wt) = self.worktrees.iter().find(|w| w.admin_dir.as_deref() == Some(&admin)) {
                        targets.insert(wt.path.clone());
                    }
                }
            } else if first != "refs" && first != "packed-refs" {
                // HEAD, index, MERGE_HEAD, ... of the main worktree.
                if let Some(wt) = self.worktrees.iter().find(|w| w.admin_dir.as_deref() == Some(&common)) {
                    targets.insert(wt.path.clone());
                }
            }
        }
        for (root, dir) in new_dirs {
            self.watch_new_dir(&root, &dir);
        }

        // Edits: right away unless this worktree was checked very recently.
        let now = Instant::now();
        for root in edited {
            match self.last_status.get(&root) {
                Some(&t) if now.duration_since(t) < MIN_GAP => {
                    self.deferred.entry(root).or_insert(t + MIN_GAP);
                }
                _ => {
                    targets.insert(root);
                }
            }
        }

        let mut changed = false;
        if git_relevant && self.refresh_refs(false) {
            changed = true;
            self.after_worktree_list_change(&mut targets);
        }
        if !targets.is_empty() {
            changed |= self.refresh_status(Some(&targets));
        }
        if changed {
            self.refresh_graph();
            self.refresh_changes();
            self.publish();
        }
    }

    /// Reload worktree list and refs; returns whether anything changed.
    fn refresh_refs(&mut self, force: bool) -> bool {
        let dir = self.repo_dir().to_path_buf();
        let listed = git::worktree::list(&dir).and_then(|wl| Ok((wl, git::refs::load(&dir)?)));
        let ((entries, raw_wt), (refs, raw_refs)) = match listed {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return false;
            }
        };
        self.error = None;
        let mut h = DefaultHasher::new();
        raw_wt.hash(&mut h);
        raw_refs.hash(&mut h);
        let fp = h.finish();
        if fp == self.fingerprint && !force && !self.worktrees.is_empty() {
            return false;
        }
        self.fingerprint = fp;

        let mut old: HashMap<PathBuf, WorktreeView> = self.worktrees.drain(..).map(|w| (w.path.clone(), w)).collect();
        let current = self.paths.toplevel.as_deref();
        let mut views: Vec<WorktreeView> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let mut v = WorktreeView::from_entry(e, i == 0, current);
                if let Some(prev) = old.remove(&v.path) {
                    // Keep the last status until the next status run replaces it.
                    v.status = prev.status;
                    v.status_error = prev.status_error;
                    v.op = prev.op;
                    v.changed_at = prev.changed_at;
                    v.admin_dir = prev.admin_dir;
                    v.changes = prev.changes;
                    v.overlaps = prev.overlaps;
                    v.landed = prev.landed;
                }
                if v.admin_dir.is_none() && v.has_worktree() {
                    v.admin_dir = git::worktree::admin_dir(&v.path);
                }
                v
            })
            .collect();

        self.base = model::detect_base(self.cfg.base.as_deref(), &refs, &entries);
        let base_sha = self.base.as_ref().and_then(|b| {
            refs.iter()
                .find(|r| r.kind == git::refs::RefKind::Branch && &r.name == b)
                .or_else(|| refs.iter().find(|r| &r.name == b))
                .map(|r| r.target.clone())
        });
        self.base_sha = base_sha.clone();
        if let Some(base_sha) = base_sha {
            let missing: Vec<String> = views
                .iter()
                .filter_map(|v| v.head.clone())
                .filter(|h| *h != base_sha && !self.ab_cache.contains_key(&(base_sha.clone(), h.clone())))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let results = par_map(&missing, PARALLELISM, |head| {
                git::left_right_count(&dir, &base_sha, head).ok()
            });
            for (head, res) in missing.into_iter().zip(results) {
                if let Some((behind, ahead)) = res {
                    self.ab_cache.insert((base_sha.clone(), head), (ahead, behind));
                }
            }
            for v in &mut views {
                v.vs_base = match &v.head {
                    Some(h) if *h == base_sha => Some((0, 0)),
                    Some(h) => self.ab_cache.get(&(base_sha.clone(), h.clone())).copied(),
                    None => None,
                };
            }
        }
        if self.ab_cache.len() > 4096 {
            self.ab_cache.clear();
        }

        let missing: Vec<String> = views
            .iter()
            .filter_map(|v| v.head.clone())
            .filter(|h| !self.head_cache.contains_key(h))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if self.head_cache.len() > 4096 {
            self.head_cache.clear();
        }
        if let Ok(described) = git::log::describe(&dir, &missing) {
            for (id, time, _) in described {
                self.head_cache.insert(id, time);
            }
        }
        for v in &mut views {
            if let Some(time) = v.head.as_ref().and_then(|h| self.head_cache.get(h)) {
                v.head_time = Some(*time);
            }
        }
        self.worktrees = views;
        self.refs = refs;
        true
    }

    /// Run `git status` for the given worktrees (all when `None`), in parallel. Returns whether
    /// anything visible changed.
    fn refresh_status(&mut self, only: Option<&HashSet<PathBuf>>) -> bool {
        self.refresh_status_with(only, PARALLELISM)
    }

    fn refresh_status_with(&mut self, only: Option<&HashSet<PathBuf>>, parallelism: usize) -> bool {
        let idx: Vec<usize> = (0..self.worktrees.len())
            .filter(|&i| {
                let w = &self.worktrees[i];
                w.has_worktree() && only.is_none_or(|s| s.contains(&w.path))
            })
            .collect();
        let paths: Vec<PathBuf> = idx.iter().map(|&i| self.worktrees[i].path.clone()).collect();
        let results = par_map(&paths, parallelism, |p| git::status::load(p));
        let now = Instant::now();
        let mut changed = false;
        for (i, res) in idx.into_iter().zip(results) {
            let period = self.period(&self.worktrees[i].path);
            let w = &mut self.worktrees[i];
            // A status run near a poll slot makes that poll redundant: skip to the next slot,
            // keeping the slot's phase so polls stay spread out.
            self.last_status.insert(w.path.clone(), now);
            if let Some(due) = self.due.get_mut(&w.path) {
                while *due < now + period / 2 {
                    *due += period;
                }
            }
            let before = (w.status.clone(), w.changed_at, w.status_error.clone(), w.op);
            match res {
                Ok(st) => {
                    w.changed_at = last_edited(&w.path, &st.paths);
                    w.status = Some(st);
                    w.status_error = None;
                }
                Err(e) => w.status_error = Some(format!("{e:#}")),
            }
            w.op = w.admin_dir.as_deref().and_then(git::worktree::operation);
            changed |= before != (w.status.clone(), w.changed_at, w.status_error.clone(), w.op);
        }
        changed
    }

    /// Reload the log if refs or the limit changed, and re-layout if anything it depends on
    /// (commits, refs, worktree HEADs or dirtiness) changed.
    fn refresh_graph(&mut self) {
        let key = (self.fingerprint, self.limit);
        if self.commits_key != Some(key) {
            let mut heads: Vec<String> = self.worktrees.iter().filter_map(|w| w.head.clone()).collect();
            heads.sort();
            heads.dedup();
            match git::log::load(self.repo_dir(), self.limit, &heads) {
                Ok(c) => {
                    self.commits = Arc::new(c);
                    self.commits_key = Some(key);
                }
                Err(e) => {
                    // An empty repository has nothing to log; anything else is worth showing.
                    if !self.refs.is_empty() {
                        self.error = Some(format!("{e:#}"));
                    }
                    self.commits = Arc::default();
                }
            }
        }
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        self.commits.len().hash(&mut h);
        for w in &self.worktrees {
            (&w.path, &w.name, &w.head, w.is_dirty()).hash(&mut h);
        }
        let gk = h.finish();
        if self.graph_key != Some(gk) {
            let more = self.commits.len() >= self.limit;
            self.graph = Arc::new(Graph::build(
                &self.commits,
                &self.refs,
                &self.worktrees,
                self.base.as_deref(),
                more,
            ));
            self.graph_key = Some(gk);
        }
    }

    /// Recompute what each worktree changed since it forked from the base branch, and which
    /// worktrees changed the same files. Git work is cached (merge bases and commit-to-commit
    /// diffs never change; working-tree counts are keyed by status). Returns whether anything
    /// visible changed.
    fn refresh_changes(&mut self) -> bool {
        let dir = self.repo_dir().to_path_buf();
        for cache_len in [
            self.mb_cache.len(),
            self.between_cache.len(),
            self.dirty_cache.len(),
            self.landed_cache.len(),
        ] {
            if cache_len > 4096 {
                self.mb_cache.clear();
                self.between_cache.clear();
                self.dirty_cache.clear();
                self.landed_cache.clear();
            }
        }
        let Some(base_sha) = self.base_sha.clone() else {
            let mut changed = false;
            for w in &mut self.worktrees {
                changed |= w.changes.take().is_some()
                    || !std::mem::take(&mut w.overlaps).is_empty()
                    || w.landed.take().is_some();
            }
            return changed;
        };

        // Everything that takes part: worktrees with a HEAD and a status, plus the base branch
        // itself when no worktree has it checked out.
        struct Side {
            wi: Option<usize>,
            name: String,
            path: Option<PathBuf>,
            head: String,
            /// Paths `git status` reports, untracked included.
            dirty: Vec<String>,
            is_base: bool,
        }
        let base_name = self.base.clone().unwrap_or_default();
        let mut sides: Vec<Side> = self
            .worktrees
            .iter()
            .enumerate()
            .filter(|(_, w)| w.has_worktree())
            .filter_map(|(wi, w)| {
                Some(Side {
                    wi: Some(wi),
                    name: w.name.clone(),
                    path: Some(w.path.clone()),
                    head: w.head.clone()?,
                    dirty: w.status.as_ref()?.paths.clone(),
                    is_base: w.branch.as_deref() == Some(&base_name),
                })
            })
            .collect();
        if !sides.iter().any(|s| s.is_base) {
            sides.push(Side {
                wi: None,
                name: base_name,
                path: None,
                head: base_sha.clone(),
                dirty: Vec::new(),
                is_base: true,
            });
        }

        // Where each side forked from the base branch, and where each pair's histories split.
        let mut wanted: Vec<(String, String)> = sides.iter().map(|s| (base_sha.clone(), s.head.clone())).collect();
        for (i, a) in sides.iter().enumerate() {
            for b in &sides[i + 1..] {
                wanted.push((a.head.clone(), b.head.clone()));
            }
        }
        self.fill_merge_bases(&dir, wanted);
        let fork: Vec<Option<String>> = sides.iter().map(|s| self.merge_base(&base_sha, &s.head)).collect();
        let mut pairs: Vec<(usize, usize, String)> = Vec::new();
        for i in 0..sides.len() {
            for j in i + 1..sides.len() {
                if let Some(x) = self.merge_base(&sides[i].head, &sides[j].head) {
                    pairs.push((i, j, x));
                }
            }
        }
        // In a pair, a side's own work starts at the later of the split point and its fork from
        // base: what it got from base isn't its work (unless it's the base branch itself), and
        // what it shares with the other side (one built on the other) isn't either.
        let wanted: Vec<(String, String)> = pairs
            .iter()
            .flat_map(|(i, j, x)| [(*i, x), (*j, x)])
            .filter_map(|(k, x)| Some((x.clone(), fork[k].clone()?)))
            .filter(|(x, f)| x != f)
            .collect();
        self.fill_merge_bases(&dir, wanted);
        let start = |k: usize, x: &String| -> String {
            match &fork[k] {
                Some(f) if !sides[k].is_base && f != x && self.merge_base(x, f).as_ref() == Some(x) => f.clone(),
                _ => x.clone(),
            }
        };
        let pairs: Vec<(usize, usize, String, String)> = pairs
            .iter()
            .map(|(i, j, x)| (*i, *j, start(*i, x), start(*j, x)))
            .collect();

        // Commit-to-commit diffs: fork → HEAD for the sizes, own-work start → HEAD for overlaps.
        let mut wanted: HashSet<(String, String)> = HashSet::new();
        for (s, f) in sides.iter().zip(&fork) {
            if let Some(f) = f {
                wanted.insert((f.clone(), s.head.clone()));
            }
        }
        for (i, j, from_i, from_j) in &pairs {
            wanted.insert((from_i.clone(), sides[*i].head.clone()));
            wanted.insert((from_j.clone(), sides[*j].head.clone()));
        }
        let missing: Vec<(String, String)> = wanted
            .into_iter()
            .filter(|k| !self.between_cache.contains_key(k))
            .collect();
        let found = par_map(&missing, PARALLELISM, |(from, to)| {
            git::changes::between(&dir, from, to).ok()
        });
        for (k, v) in missing.into_iter().zip(found) {
            if let Some(v) = v {
                self.between_cache.insert(k, Arc::new(v));
            }
        }

        // Uncommitted part: merge base → working tree, for the paths status reports (all paths
        // when status had more than it lists).
        let mut jobs: Vec<(PathBuf, String, Option<Vec<String>>, u64)> = Vec::new();
        for (s, f) in sides.iter().zip(&fork) {
            let (Some(wi), Some(f)) = (s.wi, f) else { continue };
            let w = &self.worktrees[wi];
            if !w.is_dirty() {
                self.dirty_cache.remove(&w.path);
                continue;
            }
            let mut h = DefaultHasher::new();
            (f, &w.status, w.changed_at).hash(&mut h);
            let key = h.finish();
            if self.dirty_cache.get(&w.path).is_some_and(|(k, _)| *k == key) {
                continue;
            }
            let truncated = s.dirty.len() >= git::status::MAX_PATHS;
            jobs.push((w.path.clone(), f.clone(), (!truncated).then(|| s.dirty.clone()), key));
        }
        let found = par_map(&jobs, PARALLELISM, |(path, from, paths, _)| {
            git::changes::worktree(path, from, paths.as_deref()).ok()
        });
        for ((path, _, _, key), v) in jobs.into_iter().zip(found) {
            if let Some(v) = v {
                self.dirty_cache.insert(path, (key, Arc::new(v)));
            }
        }

        // Per worktree: committed + uncommitted counts.
        let mut changes: Vec<Option<Arc<Changes>>> = vec![None; self.worktrees.len()];
        for (s, f) in sides.iter().zip(&fork) {
            let (Some(wi), Some(f)) = (s.wi, f) else { continue };
            let Some(committed) = self.between_cache.get(&(f.clone(), s.head.clone())) else {
                continue;
            };
            let w = &self.worktrees[wi];
            let ch = match self.dirty_cache.get(&w.path).filter(|_| w.is_dirty()) {
                Some((_, dirty)) => {
                    let all = s.dirty.len() >= git::status::MAX_PATHS;
                    Changes::merge(f.clone(), committed, dirty, (!all).then_some(&s.dirty[..]))
                }
                None => Changes::merge(f.clone(), committed, &[], Some(&[])),
            };
            changes[wi] = Some(match &w.changes {
                Some(old) if **old == ch => old.clone(),
                _ => Arc::new(ch),
            });
        }

        // Whether each worktree's work is already in the base branch. HEAD in base: merged if
        // the branch ever moved (its reflog has more than the creation entry), else unused.
        // HEAD not in base: applied if every file it changed since forking is the same there.
        enum Check {
            Reflog(String),
            Content(Vec<String>),
        }
        let mut checks: Vec<(LandedKey, Check)> = Vec::new();
        for (s, f) in sides.iter().zip(&fork) {
            let (Some(wi), Some(f)) = (s.wi, f) else { continue };
            let w = &self.worktrees[wi];
            let Some((ahead, _)) = w.vs_base.filter(|_| !s.is_base) else { continue };
            let key = (base_sha.clone(), s.head.clone(), w.branch.clone());
            if self.landed_cache.contains_key(&key) {
                continue;
            }
            let check = if ahead == 0 {
                let Some(b) = &w.branch else { continue };
                Check::Reflog(b.clone())
            } else {
                let Some(committed) = self.between_cache.get(&(f.clone(), s.head.clone())) else {
                    continue;
                };
                if committed.is_empty() || committed.len() > git::changes::MAX_APPLIED_FILES {
                    self.landed_cache.insert(key, None);
                    continue;
                }
                Check::Content(committed.iter().map(|c| c.path.clone()).collect())
            };
            checks.push((key, check));
        }
        let found = par_map(&checks, PARALLELISM, |((base, head, _), check)| match check {
            Check::Reflog(b) => match git::changes::reflog_len(&dir, b, 2) {
                Ok(n) if n >= 2 => Some(Landed::Merged),
                Ok(1) => Some(Landed::Unused),
                _ => None,
            },
            Check::Content(paths) => git::changes::same_content(&dir, head, base, paths)
                .ok()
                .and_then(|same| same.then_some(Landed::Applied)),
        });
        for ((key, _), v) in checks.into_iter().zip(found) {
            self.landed_cache.insert(key, v);
        }
        let mut landed: Vec<Option<Landed>> = vec![None; self.worktrees.len()];
        for s in &sides {
            let Some(wi) = s.wi else { continue };
            let key = (base_sha.clone(), s.head.clone(), self.worktrees[wi].branch.clone());
            landed[wi] = self.landed_cache.get(&key).copied().flatten();
        }

        // Overlaps: per pair, the files both sides changed in their own work (committed or not).
        let touched = |s: &Side, from: &str| -> Option<Vec<String>> {
            let committed = self.between_cache.get(&(from.to_string(), s.head.clone()))?;
            let mut v: Vec<String> = committed.iter().map(|f| f.path.clone()).collect();
            v.extend(s.dirty.iter().cloned());
            Some(v)
        };
        let mut overlaps: Vec<Vec<Overlap>> = vec![Vec::new(); self.worktrees.len()];
        // A merged worktree's work is in base already: overlapping it means nothing.
        let merged = |s: &Side| s.wi.is_some_and(|wi| matches!(landed[wi], Some(Landed::Merged | Landed::Applied)));
        for (i, j, from_i, from_j) in &pairs {
            let (a, b) = (&sides[*i], &sides[*j]);
            if merged(a) || merged(b) {
                continue;
            }
            let (Some(ta), Some(tb)) = (touched(a, from_i), touched(b, from_j)) else {
                continue;
            };
            let files = model::shared_files(&ta, &tb);
            if files.is_empty() {
                continue;
            }
            for (me, other, me_from, other_from) in [(a, b, from_i, from_j), (b, a, from_j, from_i)] {
                if let Some(wi) = me.wi {
                    overlaps[wi].push(Overlap {
                        with: other.name.clone(),
                        path: other.path.clone(),
                        files: files.clone(),
                        from: me_from.clone(),
                        other_from: other_from.clone(),
                        other_head: other.head.clone(),
                    });
                }
            }
        }

        let mut changed = false;
        for (((w, ch), mut ov), l) in self.worktrees.iter_mut().zip(changes).zip(overlaps).zip(landed) {
            ov.sort_by(|a, b| b.files.len().cmp(&a.files.len()).then_with(|| a.with.cmp(&b.with)));
            changed |= w.changes != ch || w.overlaps != ov || w.landed != l;
            w.changes = ch;
            w.overlaps = ov;
            w.landed = l;
        }
        changed
    }

    /// Compute the merge bases of `pairs` that aren't cached yet (in parallel).
    fn fill_merge_bases(&mut self, dir: &Path, pairs: Vec<(String, String)>) {
        let missing: Vec<(String, String)> = pairs
            .into_iter()
            .map(|(a, b)| sorted_pair(a, b))
            .filter(|k| k.0 != k.1 && !self.mb_cache.contains_key(k))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let found = par_map(&missing, PARALLELISM, |(a, b)| {
            git::changes::merge_base(dir, a, b).unwrap_or(None)
        });
        self.mb_cache.extend(missing.into_iter().zip(found));
    }

    fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        if a == b {
            return Some(a.to_string());
        }
        self.mb_cache
            .get(&sorted_pair(a.to_string(), b.to_string()))
            .cloned()
            .flatten()
    }

    fn publish(&self) {
        let repo_name = self
            .worktrees
            .first()
            .and_then(|w| w.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.paths.common_dir.display().to_string());
        (self.emit)(Arc::new(Snapshot {
            repo_name,
            common_dir: self.paths.common_dir.clone(),
            base: self.base.clone(),
            worktrees: self
                .worktrees
                .iter()
                .map(|w| WorktreeView {
                    watched: self.trees.contains_key(&w.path),
                    ..w.clone()
                })
                .collect(),
            graph: self.graph.clone(),
            watching: self.watcher.is_some(),
            polling: self.cfg.poll,
            statuses_loaded: self.statuses_loaded,
            error: self.error.clone(),
        }));
    }
}

/// The ignored dirs and the directories to watch for a worktree (see [`trees::dirs_to_watch`]);
/// `None` if it is on a filesystem without working events or too big to watch.
fn plan_tree(root: &Path) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
    if fsinfo::events_at(root) == Events::Unreliable {
        return None;
    }
    let ignored = trees::ignored_dirs(root, root);
    let dirs = trees::dirs_to_watch(root, &ignored, MAX_WATCH_DIRS)?;
    Some((ignored, dirs))
}

/// Watch `dirs`; `None` (and nothing left watched) if any watch fails, e.g. the inotify limit.
fn watch_dirs(watcher: &mut RecommendedWatcher, ignored: Vec<PathBuf>, dirs: Vec<PathBuf>) -> Option<TreeWatch> {
    let mut tw = TreeWatch {
        dirs: HashSet::new(),
        ignored,
    };
    for d in dirs {
        if watcher.watch(&d, RecursiveMode::NonRecursive).is_err() {
            for w in &tw.dirs {
                let _ = watcher.unwatch(w);
            }
            return None;
        }
        tw.dirs.insert(d);
    }
    Some(tw)
}

/// (base sha, HEAD, branch).
type LandedKey = (String, String, Option<String>);

fn sorted_pair(a: String, b: String) -> (String, String) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Newest mtime among the changed paths (deleted files are skipped), as unix seconds.
fn last_edited(root: &Path, paths: &[String]) -> Option<i64> {
    paths
        .iter()
        .filter_map(|p| std::fs::symlink_metadata(root.join(p)).ok()?.modified().ok())
        .max()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Map `f` over `items` with at most `max` threads, preserving order.
fn par_map<T: Sync, R: Send>(items: &[T], max: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let n = max.min(items.len());
    if n <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    out.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    out.into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("par_map slot filled"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par_map_preserves_order() {
        let v: Vec<u32> = (0..50).collect();
        assert_eq!(par_map(&v, 4, |x| x * 2), v.iter().map(|x| x * 2).collect::<Vec<_>>());
        assert_eq!(par_map(&[] as &[u32], 4, |x| *x), Vec::<u32>::new());
    }
}
