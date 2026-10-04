//! Background refresh: polling + filesystem watching.
//!
//! One worker thread owns all repository state. It polls every worktree's status on an interval
//! (working-tree edits never touch `.git`), and reacts quickly to fs events in the common git dir
//! (refs, HEADs, indexes). Each change produces a new immutable [`Snapshot`] handed to `emit`.
//! Refs are fingerprinted so the log/graph is only recomputed when something actually moved.

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

use crate::git::{self, RepoPaths, log::Commit, refs::RefEntry};
use crate::graph::Graph;
use crate::model::{self, Snapshot, WorktreeView};

/// Coalesce bursts of fs events (git writes several files per operation).
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Max concurrent git processes for per-worktree work.
const PARALLELISM: usize = 6;
/// Commits loaded initially and per "load more" step.
pub const GRAPH_PAGE: usize = 1000;

#[derive(Clone, Debug)]
pub struct Config {
    pub interval: Duration,
    /// Use fs notifications in addition to polling.
    pub watch: bool,
    /// Branch to compare worktrees against; auto-detected when `None`.
    pub base: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval: Duration::from_millis(1500),
            watch: true,
            base: None,
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

    worktrees: Vec<WorktreeView>,
    refs: Vec<RefEntry>,
    /// Hash of raw `worktree list` + `for-each-ref` output.
    fingerprint: u64,
    base: Option<String>,
    ab_cache: HashMap<(String, String), (u32, u32)>,

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
        Worker {
            paths,
            cfg,
            emit,
            watcher: None,
            watched_worktrees_dir: false,
            worktrees: Vec::new(),
            refs: Vec::new(),
            fingerprint: 0,
            base: None,
            ab_cache: HashMap::new(),
            limit: GRAPH_PAGE,
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
        let handler = move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res
                && !ev.paths.is_empty()
            {
                let _ = tx.send(Msg::Fs(ev.paths));
            }
        };
        let Ok(mut watcher) = notify::recommended_watcher(handler) else {
            return; // Polling still covers everything, just with more latency.
        };
        let common = self.paths.common_dir.clone();
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

    fn initial_load(&mut self) {
        // Worktree list first (fast), then statuses, then the graph: the overview fills in
        // progressively instead of waiting on the slowest piece.
        self.refresh_refs(false);
        self.publish();
        self.refresh_status(None);
        self.statuses_loaded = true;
        self.publish();
        self.refresh_graph();
        self.publish();
    }

    fn run(&mut self, rx: mpsc::Receiver<Msg>) {
        let mut next_poll = Instant::now() + self.cfg.interval;
        let mut pending: Vec<PathBuf> = Vec::new();
        let mut debounce_until: Option<Instant> = None;
        loop {
            let deadline = debounce_until.map_or(next_poll, |d| d.min(next_poll));
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Msg::Fs(paths)) => {
                    pending.extend(paths);
                    debounce_until.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                }
                Ok(Msg::Req(Request::Refresh)) => {
                    self.full_refresh();
                    next_poll = Instant::now() + self.cfg.interval;
                }
                Ok(Msg::Req(Request::LoadMore)) => {
                    if self.commits.len() >= self.limit {
                        self.limit += GRAPH_PAGE;
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
            if now >= next_poll {
                self.full_refresh();
                next_poll = Instant::now() + self.cfg.interval;
            }
        }
    }

    fn full_refresh(&mut self) {
        self.refresh_refs(false);
        self.refresh_status(None);
        self.refresh_graph();
        self.publish();
    }

    fn on_fs_events(&mut self, paths: Vec<PathBuf>) {
        let common = self.paths.common_dir.clone();
        let mut targets: HashSet<PathBuf> = HashSet::new();
        let mut relevant = false;
        for p in &paths {
            let Ok(rel) = p.strip_prefix(&common) else {
                continue;
            };
            let mut comps = rel.components().map(|c| c.as_os_str().to_string_lossy());
            let first = comps.next().unwrap_or_default();
            if p.extension().is_some_and(|e| e == "lock")
                || matches!(
                    &*first,
                    "objects" | "logs" | "COMMIT_EDITMSG" | "gc.log" | "FETCH_HEAD"
                )
            {
                continue;
            }
            relevant = true;
            if first == "worktrees" {
                if let Some(name) = comps.next() {
                    let admin = common.join("worktrees").join(&*name);
                    if comps.next().is_some_and(|c| c == "logs") {
                        continue;
                    }
                    if let Some(wt) = self
                        .worktrees
                        .iter()
                        .find(|w| w.admin_dir.as_deref() == Some(&admin))
                    {
                        targets.insert(wt.path.clone());
                    }
                }
            } else if first != "refs" && first != "packed-refs" {
                // HEAD, index, MERGE_HEAD, ... of the main worktree.
                if let Some(wt) = self
                    .worktrees
                    .iter()
                    .find(|w| w.admin_dir.as_deref() == Some(&common))
                {
                    targets.insert(wt.path.clone());
                }
            }
        }
        if !relevant {
            return;
        }
        self.ensure_worktrees_watch();
        let changed = self.refresh_refs(false);
        // New worktrees (no status yet) need one too.
        targets.extend(
            self.worktrees
                .iter()
                .filter(|w| w.status.is_none() && w.has_worktree())
                .map(|w| w.path.clone()),
        );
        if !targets.is_empty() {
            self.refresh_status(Some(&targets));
        }
        if changed || !targets.is_empty() {
            self.refresh_graph();
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

        let mut old: HashMap<PathBuf, WorktreeView> = self
            .worktrees
            .drain(..)
            .map(|w| (w.path.clone(), w))
            .collect();
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
                    v.admin_dir = prev.admin_dir;
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
        if let Some(base_sha) = base_sha {
            let missing: Vec<String> = views
                .iter()
                .filter_map(|v| v.head.clone())
                .filter(|h| {
                    *h != base_sha && !self.ab_cache.contains_key(&(base_sha.clone(), h.clone()))
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let results = par_map(&missing, PARALLELISM, |head| {
                git::left_right_count(&dir, &base_sha, head).ok()
            });
            for (head, res) in missing.into_iter().zip(results) {
                if let Some((behind, ahead)) = res {
                    self.ab_cache
                        .insert((base_sha.clone(), head), (ahead, behind));
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
        self.worktrees = views;
        self.refs = refs;
        true
    }

    /// Run `git status` for the given worktrees (all when `None`), in parallel.
    fn refresh_status(&mut self, only: Option<&HashSet<PathBuf>>) {
        let idx: Vec<usize> = (0..self.worktrees.len())
            .filter(|&i| {
                let w = &self.worktrees[i];
                w.has_worktree() && only.is_none_or(|s| s.contains(&w.path))
            })
            .collect();
        let paths: Vec<PathBuf> = idx
            .iter()
            .map(|&i| self.worktrees[i].path.clone())
            .collect();
        let results = par_map(&paths, PARALLELISM, |p| git::status::load(p));
        for (i, res) in idx.into_iter().zip(results) {
            let w = &mut self.worktrees[i];
            match res {
                Ok(st) => {
                    w.status = Some(st);
                    w.status_error = None;
                }
                Err(e) => w.status_error = Some(format!("{e:#}")),
            }
            w.op = w.admin_dir.as_deref().and_then(git::worktree::operation);
        }
    }

    /// Reload the log if refs or the limit changed, and re-layout if anything it depends on
    /// (commits, refs, worktree HEADs or dirtiness) changed.
    fn refresh_graph(&mut self) {
        let key = (self.fingerprint, self.limit);
        if self.commits_key != Some(key) {
            let mut heads: Vec<String> = self
                .worktrees
                .iter()
                .filter_map(|w| w.head.clone())
                .collect();
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
                more,
            ));
            self.graph_key = Some(gk);
        }
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
            worktrees: self.worktrees.clone(),
            graph: self.graph.clone(),
            watching: self.watcher.is_some(),
            statuses_loaded: self.statuses_loaded,
            error: self.error.clone(),
        }));
    }
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
        assert_eq!(
            par_map(&v, 4, |x| x * 2),
            v.iter().map(|x| x * 2).collect::<Vec<_>>()
        );
        assert_eq!(par_map(&[] as &[u32], 4, |x| *x), Vec::<u32>::new());
    }
}
