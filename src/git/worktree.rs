//! `git worktree list --porcelain -z` and per-worktree admin dirs.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{lossy, non_null_oid, split_nul};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// HEAD commit; `None` for an unborn branch or a bare entry.
    pub head: Option<String>,
    /// Full ref name (`refs/heads/x`) when on a branch.
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

impl WorktreeEntry {
    /// Short branch name (`x` for `refs/heads/x`).
    pub fn branch_name(&self) -> Option<&str> {
        self.branch
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
    }
}

pub fn list(dir: &Path) -> Result<(Vec<WorktreeEntry>, Vec<u8>)> {
    let raw = super::run(dir, &["worktree", "list", "--porcelain", "-z"])?;
    Ok((parse(&raw), raw))
}

/// Parse porcelain -z output: attribute records separated by NUL, worktrees by an empty record.
pub fn parse(bytes: &[u8]) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for rec in split_nul(bytes) {
        if rec.is_empty() {
            out.extend(cur.take());
            continue;
        }
        let rec = lossy(rec);
        let (key, val) = match rec.split_once(' ') {
            Some((k, v)) => (k, Some(v)),
            None => (rec.as_str(), None),
        };
        if key == "worktree" {
            out.extend(cur.take());
            cur = Some(WorktreeEntry {
                path: PathBuf::from(val.unwrap_or_default()),
                ..Default::default()
            });
            continue;
        }
        let Some(wt) = cur.as_mut() else { continue };
        match key {
            "HEAD" => wt.head = val.and_then(non_null_oid),
            "branch" => wt.branch = val.map(str::to_string),
            "detached" => wt.detached = true,
            "bare" => wt.bare = true,
            "locked" => wt.locked = Some(val.unwrap_or_default().to_string()),
            "prunable" => wt.prunable = Some(val.unwrap_or_default().to_string()),
            _ => {}
        }
    }
    out.extend(cur);
    out
}

/// The worktree's private git dir: `.git` itself for the main worktree, or the
/// `<common>/worktrees/<name>` dir that a linked worktree's `.git` file points to.
pub fn admin_dir(wt_path: &Path) -> Option<PathBuf> {
    let dotgit = wt_path.join(".git");
    let meta = std::fs::metadata(&dotgit).ok()?;
    if meta.is_dir() {
        return Some(super::canonical(&dotgit));
    }
    let text = std::fs::read_to_string(&dotgit).ok()?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    Some(super::canonical(&wt_path.join(target)))
}

/// When git last did something in a linked worktree (unix seconds): the newest mtime of its admin
/// dir's `HEAD`, `index` (written by checkouts, adds and index refreshes, e.g. an agent's `git
/// status`) and `commondir` (written once, by `worktree add`).
pub fn last_used(admin: &Path) -> Option<i64> {
    ["HEAD", "index", "commondir"]
        .iter()
        .filter_map(|f| std::fs::metadata(admin.join(f)).and_then(|m| m.modified()).ok())
        .max()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// An in-progress operation (merge, rebase, ...) detected from files in the admin dir.
pub fn operation(admin: &Path) -> Option<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("rebase-merge", "REBASING"),
        ("rebase-apply", "REBASING"),
        ("MERGE_HEAD", "MERGING"),
        ("CHERRY_PICK_HEAD", "PICKING"),
        ("REVERT_HEAD", "REVERTING"),
        ("BISECT_LOG", "BISECTING"),
    ];
    MARKERS
        .iter()
        .find(|(file, _)| admin.join(file).exists())
        .map(|&(_, label)| label)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"worktree /r/repo\0HEAD e7dc7c6d0faa9b009e685857997dcb253df8b769\0branch refs/heads/main\0\0\
worktree /r/wt/detached\0HEAD 86a259f49a7763a11f805818597bb6a81c4806c0\0detached\0\0\
worktree /r/wt/refactor-db\0HEAD 2d7b362ce7d42825c77a13f3ce71f06ff284dde8\0branch refs/heads/refactor-db\0locked agent running\0\0\
worktree /r/wt/gone\0HEAD e7dc7c6d0faa9b009e685857997dcb253df8b769\0branch refs/heads/gone\0prunable gitdir file points to non-existent location\0\0\
worktree /r/wt/new\0HEAD 0000000000000000000000000000000000000000\0branch refs/heads/new\0\0";

    #[test]
    fn parses_all_states() {
        let wts = parse(SAMPLE);
        assert_eq!(wts.len(), 5);
        assert_eq!(wts[0].path, PathBuf::from("/r/repo"));
        assert_eq!(wts[0].branch_name(), Some("main"));
        assert!(wts[1].detached && wts[1].branch.is_none());
        assert_eq!(wts[2].locked.as_deref(), Some("agent running"));
        assert!(wts[3].prunable.is_some());
        assert_eq!(wts[4].head, None, "unborn HEAD");
    }

    #[test]
    fn parses_bare_and_missing_terminator() {
        let wts = parse(b"worktree /r/bare.git\0bare\0\0worktree /r/a\0HEAD abc\0detached");
        assert_eq!(wts.len(), 2);
        assert!(wts[0].bare);
        assert_eq!(wts[1].head.as_deref(), Some("abc"));
    }
}
