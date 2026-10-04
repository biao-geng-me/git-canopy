//! Snapshot types shared between the refresh thread and the UI.
//!
//! The refresh thread builds immutable [`Snapshot`]s and sends them over a channel; the UI only
//! renders the latest one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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
