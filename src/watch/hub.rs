//! One fs watcher shared by every repository's worker (one inotify instance, one budget of
//! watched directories), with each event routed to the worker that watches its directory.
//!
//! Lock order: never hold `routes` while calling into the watcher. notify delivers events on its
//! own thread, which takes `routes`; a `watch` call waits for that thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::Msg;

/// Watched directories for all repositories together (inotify's per-user limit is shared with
/// every other program; this stays well below it).
const MAX_TOTAL_DIRS: usize = 50_000;

#[derive(Default)]
struct Routes {
    /// Watched directory → the workers watching it (one, unless two repositories overlap).
    owners: HashMap<PathBuf, Vec<usize>>,
    /// Where each worker's events go.
    senders: HashMap<usize, Sender<Msg>>,
    next_id: usize,
}

impl Routes {
    /// Who gets an event about `path`: the owners of the nearest watched directory at or above
    /// it (a file in a watched directory, a directory itself, anything under a recursive watch).
    fn owners_of(&self, path: &Path) -> &[usize] {
        path.ancestors()
            .find_map(|a| self.owners.get(a))
            .map_or(&[], Vec::as_slice)
    }
}

pub struct Hub {
    watcher: Mutex<Option<RecommendedWatcher>>,
    routes: Arc<Mutex<Routes>>,
    budget: usize,
}

impl Hub {
    /// A hub without a working watcher (none could be created) only hands out failing watches;
    /// everything is polled then.
    pub fn new() -> Arc<Hub> {
        let routes: Arc<Mutex<Routes>> = Arc::default();
        let r = routes.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            // Access events include our own `git status` opening files: never react to them.
            let Ok(ev) = res else { return };
            if ev.kind.is_access() || ev.paths.is_empty() {
                return;
            }
            let routes = r.lock().unwrap();
            let mut per: HashMap<usize, Vec<PathBuf>> = HashMap::new();
            for p in ev.paths {
                for &id in routes.owners_of(&p) {
                    per.entry(id).or_default().push(p.clone());
                }
            }
            for (id, paths) in per {
                if let Some(tx) = routes.senders.get(&id) {
                    let _ = tx.send(Msg::Fs(paths));
                }
            }
        };
        let watcher = notify::recommended_watcher(handler).ok();
        let budget = max_user_watches().map_or(MAX_TOTAL_DIRS, |m| MAX_TOTAL_DIRS.min(m / 2));
        Arc::new(Hub {
            watcher: Mutex::new(watcher),
            routes,
            budget,
        })
    }

    pub fn works(&self) -> bool {
        self.watcher.lock().unwrap().is_some()
    }

    /// A worker's share of the hub: its events go to `tx`.
    pub(super) fn register(self: &Arc<Self>, tx: Sender<Msg>) -> Watches {
        let mut routes = self.routes.lock().unwrap();
        let id = routes.next_id;
        routes.next_id += 1;
        routes.senders.insert(id, tx);
        Watches { hub: self.clone(), id }
    }

    /// Watched directories left before the global budget is spent.
    fn remaining(&self) -> usize {
        self.budget.saturating_sub(self.watched())
    }

    /// How many directories are watched, across all repositories.
    pub fn watched(&self) -> usize {
        self.routes.lock().unwrap().owners.len()
    }

    fn watch(&self, id: usize, dir: &Path, mode: RecursiveMode) -> bool {
        // Route first, so no event between the watch and the routing is lost.
        let first = {
            let mut routes = self.routes.lock().unwrap();
            if routes.owners.get(dir).is_some_and(|o| o.contains(&id)) {
                return true;
            }
            let owners = routes.owners.entry(dir.to_path_buf()).or_default();
            owners.push(id);
            owners.len() == 1
        };
        let ok = !first
            || self
                .watcher
                .lock()
                .unwrap()
                .as_mut()
                .is_some_and(|w| w.watch(dir, mode).is_ok());
        if !ok {
            self.forget(id, dir);
        }
        ok
    }

    /// Drop `id`'s interest in `dir`; true when nobody else watches it.
    fn forget(&self, id: usize, dir: &Path) -> bool {
        let mut routes = self.routes.lock().unwrap();
        let Some(owners) = routes.owners.get_mut(dir) else { return false };
        owners.retain(|&o| o != id);
        let last = owners.is_empty();
        if last {
            routes.owners.remove(dir);
        }
        last
    }

    fn unwatch(&self, id: usize, dir: &Path) {
        if self.forget(id, dir)
            && let Some(w) = self.watcher.lock().unwrap().as_mut()
        {
            let _ = w.unwatch(dir);
        }
    }
}

/// One worker's watches in the shared [`Hub`]; dropping it unwatches them all.
pub struct Watches {
    hub: Arc<Hub>,
    id: usize,
}

impl Watches {
    /// Watch `dir`; false if the watch failed (e.g. inotify's limit), nothing changed then.
    pub fn watch(&self, dir: &Path, mode: RecursiveMode) -> bool {
        self.hub.watch(self.id, dir, mode)
    }

    pub fn unwatch(&self, dir: &Path) {
        self.hub.unwatch(self.id, dir);
    }

    /// Directories that may still be watched, across all repositories.
    pub fn remaining(&self) -> usize {
        self.hub.remaining()
    }
}

impl Drop for Watches {
    fn drop(&mut self) {
        let dirs: Vec<PathBuf> = {
            let mut routes = self.hub.routes.lock().unwrap();
            routes.senders.remove(&self.id);
            routes
                .owners
                .iter()
                .filter(|(_, o)| o.contains(&self.id))
                .map(|(d, _)| d.clone())
                .collect()
        };
        for d in dirs {
            self.hub.unwatch(self.id, &d);
        }
    }
}

/// `fs.inotify.max_user_watches` (Linux).
fn max_user_watches() -> Option<usize> {
    std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_to_nearest_watched_ancestor() {
        let mut r = Routes::default();
        r.owners.insert("/a".into(), vec![0]);
        r.owners.insert("/a/wt/b".into(), vec![1]);
        assert_eq!(r.owners_of(Path::new("/a/x.txt")), [0]);
        assert_eq!(r.owners_of(Path::new("/a/wt/b/src/y.rs")), [1]);
        assert_eq!(r.owners_of(Path::new("/a/wt/b")), [1]);
        assert!(r.owners_of(Path::new("/c/z")).is_empty());
    }
}
