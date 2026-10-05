//! Snapshot types shared between the refresh thread and the UI.
//!
//! The refresh thread builds immutable [`Snapshot`]s and sends them over a channel; the UI only
//! renders the latest one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::git::changes::FileChange;
use crate::git::refs::{RefEntry, RefKind};
use crate::git::status::Status;
use crate::git::worktree::WorktreeEntry;
use crate::graph::Graph;

#[derive(Clone, Debug, Default)]
pub struct WorktreeView {
    pub path: PathBuf,
    /// Branch name, or the directory name when detached.
    pub name: String,
    pub head: Option<String>,
    /// Short branch name when on a branch.
    pub branch: Option<String>,
    pub bare: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
    /// The first entry of `git worktree list` (the original checkout or bare repo).
    pub is_main: bool,
    /// The worktree canopy was started in.
    pub is_current: bool,
    pub admin_dir: Option<PathBuf>,
    /// `None` until the first status run finishes (or when it cannot run, e.g. prunable).
    pub status: Option<Status>,
    pub status_error: Option<String>,
    /// In-progress operation such as `MERGING` or `REBASING`.
    pub op: Option<&'static str>,
    /// (ahead, behind) relative to the base branch.
    pub vs_base: Option<(u32, u32)>,
    /// HEAD commit time (unix seconds).
    pub head_time: Option<i64>,
    /// Newest modification time (unix seconds) among the changed files: "last edited".
    pub changed_at: Option<i64>,
    /// Its directories are watched for edits (otherwise only polled).
    pub watched: bool,
    /// What it changed since it forked from the base branch (`None` without a base branch).
    pub changes: Option<Arc<Changes>>,
    /// Other worktrees (or the base branch) that changed some of the same files.
    pub overlaps: Vec<Overlap>,
}

impl WorktreeView {
    pub fn from_entry(entry: &WorktreeEntry, is_main: bool, current: Option<&Path>) -> Self {
        let path = crate::git::canonical(&entry.path);
        let branch = entry.branch_name().map(str::to_string);
        let name = display_name(entry);
        let is_current = current.is_some_and(|c| c == path);
        WorktreeView {
            name,
            head: entry.head.clone(),
            branch,
            bare: entry.bare,
            locked: entry.locked.clone(),
            prunable: entry.prunable.clone(),
            is_main,
            is_current,
            admin_dir: None,
            path,
            ..Default::default()
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.status.as_ref().is_some_and(Status::is_dirty)
    }

    /// Whether `git status` can run here at all.
    pub fn has_worktree(&self) -> bool {
        !self.bare && self.prunable.is_none()
    }
}

/// What a worktree changed since its merge base with the base branch: committed and
/// uncommitted changes to tracked files (untracked files aren't counted).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub merge_base: String,
    /// Sorted by path.
    pub files: Vec<FileChange>,
    pub adds: u64,
    pub dels: u64,
}

impl Changes {
    /// Combine the committed changes (merge base → HEAD) with `dirty`, the merge base → working
    /// tree counts of the paths `git status` reports (`dirty_paths`), which replace the committed
    /// counts of those paths. `dirty_paths: None` means `dirty` covers every path.
    pub fn merge(
        merge_base: String,
        committed: &[FileChange],
        dirty: &[FileChange],
        dirty_paths: Option<&[String]>,
    ) -> Changes {
        let mut files: std::collections::BTreeMap<&str, &FileChange> = match dirty_paths {
            None => Default::default(),
            Some(paths) => {
                let replaced: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
                committed
                    .iter()
                    .filter(|f| !replaced.contains(f.path.as_str()))
                    .map(|f| (f.path.as_str(), f))
                    .collect()
            }
        };
        for f in dirty {
            files.insert(&f.path, f);
        }
        let files: Vec<FileChange> = files.into_values().cloned().collect();
        Changes {
            merge_base,
            adds: files.iter().map(|f| f.adds as u64).sum(),
            dels: files.iter().map(|f| f.dels as u64).sum(),
            files,
        }
    }
}

/// Files a worktree and another worktree (or the base branch) both changed since their
/// histories split: likely merge conflicts, or two agents doing the same work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    /// The other worktree's name, or the base branch.
    pub with: String,
    /// The other worktree; `None` for the base branch when no worktree has it checked out.
    pub path: Option<PathBuf>,
    pub files: Vec<String>,
}

/// Paths in both `a` and `b`. An entry ending in `/` (an untracked directory, as `git status`
/// reports it) stands for everything under it.
pub fn shared_files(a: &[String], b: &[String]) -> Vec<String> {
    let in_b: std::collections::HashSet<&str> = b.iter().map(String::as_str).collect();
    let mut out: Vec<String> = a.iter().filter(|p| in_b.contains(p.as_str())).cloned().collect();
    // Directories are few (untracked ones only), so scanning the other side for each is cheap.
    for (dirs, other) in [(a, b), (b, a)] {
        for d in dirs.iter().filter(|d| d.ends_with('/')) {
            out.extend(other.iter().filter(|p| p.starts_with(d.as_str()) && *p != d).cloned());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Branch name if on a branch, else the worktree directory name.
pub fn display_name(entry: &WorktreeEntry) -> String {
    if entry.bare {
        return "(bare)".into();
    }
    if let Some(b) = entry.branch_name() {
        return b.to_string();
    }
    entry
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.path.display().to_string())
}

/// The branch other branches are compared against: an explicit choice, else the first of
/// main/master/trunk/develop that exists locally, else the main worktree's branch.
pub fn detect_base(explicit: Option<&str>, refs: &[RefEntry], worktrees: &[WorktreeEntry]) -> Option<String> {
    if let Some(b) = explicit {
        return Some(b.to_string());
    }
    let has = |n: &str| refs.iter().any(|r| r.kind == RefKind::Branch && r.name == n);
    ["main", "master", "trunk", "develop"]
        .into_iter()
        .find(|n| has(n))
        .map(str::to_string)
        .or_else(|| worktrees.first()?.branch_name().map(str::to_string))
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Display name of the repository (main worktree directory).
    pub repo_name: String,
    pub common_dir: PathBuf,
    pub base: Option<String>,
    pub worktrees: Vec<WorktreeView>,
    pub graph: Arc<Graph>,
    /// Whether filesystem events are being watched (otherwise polling only).
    pub watching: bool,
    /// Whether periodic polling is on.
    pub polling: bool,
    /// True once every worktree's status has been loaded at least once.
    pub statuses_loaded: bool,
    /// Last refresh error that is not specific to a worktree.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(path: &str, branch: Option<&str>) -> WorktreeEntry {
        WorktreeEntry {
            path: path.into(),
            branch: branch.map(|b| format!("refs/heads/{b}")),
            detached: branch.is_none(),
            ..Default::default()
        }
    }

    fn branch(name: &str) -> RefEntry {
        RefEntry {
            kind: RefKind::Branch,
            name: name.into(),
            target: "x".into(),
        }
    }

    #[test]
    fn names() {
        assert_eq!(display_name(&wt("/a/feat", Some("feature/x"))), "feature/x");
        assert_eq!(display_name(&wt("/a/detached-1", None)), "detached-1");
    }

    #[test]
    fn shared() {
        let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(shared_files(&v(&["a", "b", "c"]), &v(&["c", "a", "z"])), v(&["a", "c"]));
        // An untracked directory stands for the files under it, on either side.
        assert_eq!(
            shared_files(&v(&["new/", "x"]), &v(&["new/f.rs", "y"])),
            v(&["new/f.rs"])
        );
        assert_eq!(shared_files(&v(&["new/f.rs"]), &v(&["new/"])), v(&["new/f.rs"]));
        assert_eq!(shared_files(&v(&["new/"]), &v(&["new/"])), v(&["new/"]));
        assert!(shared_files(&v(&["newer"]), &v(&["new/"])).is_empty());
    }

    fn fc(path: &str, adds: u32, dels: u32) -> FileChange {
        FileChange {
            path: path.into(),
            adds,
            dels,
            binary: false,
        }
    }

    #[test]
    fn merged_changes() {
        let committed = [fc("a", 10, 2), fc("b", 5, 0)];
        // b was edited further, c is new and staged, d is untracked (no counts).
        let dirty = [fc("b", 7, 1), fc("c", 3, 0)];
        let paths = ["b".to_string(), "c".to_string(), "d".to_string()];
        let ch = Changes::merge("mb".into(), &committed, &dirty, Some(&paths));
        assert_eq!(ch.files, [fc("a", 10, 2), fc("b", 7, 1), fc("c", 3, 0)]);
        assert_eq!((ch.adds, ch.dels), (20, 3));
        // b was reverted to the merge base: gone.
        let ch = Changes::merge("mb".into(), &committed, &[], Some(&["b".to_string()]));
        assert_eq!(ch.files, [fc("a", 10, 2)]);
        // A full working-tree diff replaces everything.
        let ch = Changes::merge("mb".into(), &committed, &[fc("z", 1, 1)], None);
        assert_eq!(ch.files, [fc("z", 1, 1)]);
    }

    #[test]
    fn base_detection() {
        let wts = vec![wt("/r", Some("dev2"))];
        assert_eq!(detect_base(Some("x"), &[], &wts).as_deref(), Some("x"));
        assert_eq!(
            detect_base(None, &[branch("master"), branch("main")], &wts).as_deref(),
            Some("main")
        );
        assert_eq!(detect_base(None, &[branch("master")], &wts).as_deref(), Some("master"));
        assert_eq!(detect_base(None, &[branch("other")], &wts).as_deref(), Some("dev2"));
    }
}
