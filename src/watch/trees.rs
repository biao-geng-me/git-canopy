//! Watching worktree directories for edits, and spreading polls over the interval.
//!
//! inotify watches are per directory, so a worktree is watched by adding one non-recursive watch
//! per directory, skipping `.git`, nested repositories/worktrees and git-ignored directories
//! (`target/`, `node_modules/`, …): builds churning there must not trigger `git status`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::git;

/// Watch state of one worktree.
#[derive(Debug, Default)]
pub struct TreeWatch {
    /// Directories with a watch.
    pub dirs: HashSet<PathBuf>,
    /// Ignored directories (absolute); events under them are dropped.
    pub ignored: Vec<PathBuf>,
}

impl TreeWatch {
    /// Whether an event at `path` (inside worktree `root`) can be ignored.
    pub fn is_noise(&self, root: &Path, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(root) else { return true };
        rel.components().next().is_some_and(|c| c.as_os_str() == ".git")
            || self.ignored.iter().any(|d| path.starts_with(d))
    }
}

/// Ignored directories under `dir` (inside worktree `wt`), as absolute paths. Uses
/// `--directory`, so git doesn't descend into them.
pub fn ignored_dirs(wt: &Path, dir: &Path) -> Vec<PathBuf> {
    let rel = dir.strip_prefix(wt).unwrap_or(Path::new(""));
    let mut args = vec![
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
        "-z",
        "--",
    ];
    let rel_s = rel.to_string_lossy();
    if !rel_s.is_empty() {
        args.push(&rel_s);
    }
    let Ok(out) = git::run(wt, &args) else {
        return Vec::new();
    };
    git::split_nul(&out)
        .map(git::lossy)
        .filter_map(|p| p.strip_suffix('/').map(|d| wt.join(d)))
        .collect()
}

/// The directories to watch under `root`: all of them except `.git`, ignored ones and nested
/// repositories (directories containing `.git`). `None` if there are more than `max`.
pub fn dirs_to_watch(root: &Path, ignored: &[PathBuf], max: usize) -> Option<Vec<PathBuf>> {
    let skip: HashSet<&Path> = ignored.iter().map(PathBuf::as_path).collect();
    if skip.contains(root) {
        return Some(Vec::new());
    }
    let mut out = vec![root.to_path_buf()];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            // file_type() does not follow symlinks, so symlinked dirs are not entered.
            if !entry.file_type().is_ok_and(|t| t.is_dir()) || entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            if skip.contains(path.as_path()) || path.join(".git").exists() {
                continue;
            }
            if out.len() >= max {
                return None;
            }
            out.push(path.clone());
            stack.push(path);
        }
    }
    Some(out)
}

/// Due times for `n` polls spread evenly over `period`, the first one `period / n` from now.
pub fn stagger(now: Instant, period: Duration, n: usize) -> Vec<Instant> {
    (1..=n).map(|i| now + period.mul_f64(i as f64 / n as f64)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("canopy-trees-{}-{:?}", std::process::id(), Instant::now()));
        for d in ["src/a", "target/debug/deps", "docs", "nested/inner", ".git/objects"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("nested/.git"), "gitdir: elsewhere").unwrap();
        root
    }

    #[test]
    fn skips_git_ignored_and_nested() {
        let root = tmp_tree();
        let mut dirs = dirs_to_watch(&root, &[root.join("target")], 100).unwrap();
        dirs.sort();
        let rel: Vec<String> = dirs
            .iter()
            .map(|d| d.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        assert_eq!(rel, ["", "docs", "src", "src/a"]);
        assert!(dirs_to_watch(&root, &[], 3).is_none(), "over budget");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn noise_filter() {
        let root = PathBuf::from("/w");
        let tw = TreeWatch {
            dirs: HashSet::new(),
            ignored: vec![root.join("target")],
        };
        assert!(tw.is_noise(&root, &root.join("target/debug/x.o")));
        assert!(tw.is_noise(&root, &root.join(".git/index")));
        assert!(tw.is_noise(&root, &root.join(".git")));
        assert!(!tw.is_noise(&root, &root.join("src/main.rs")));
        assert!(!tw.is_noise(&root, &root.join("targets.txt")));
    }

    #[test]
    fn staggers_evenly() {
        let now = Instant::now();
        let due = stagger(now, Duration::from_secs(60), 4);
        let secs: Vec<u64> = due.iter().map(|d| d.duration_since(now).as_secs()).collect();
        assert_eq!(secs, [15, 30, 45, 60]);
    }
}
