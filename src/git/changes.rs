//! What a branch changed since it forked: merge bases and per-file line counts
//! (`diff-tree`/`diff-index --numstat`). Renames are off (`--no-renames`), so every entry is
//! keyed by one path and committed and uncommitted counts can be merged per path.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{lossy, split_nul};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub adds: u32,
    pub dels: u32,
    pub binary: bool,
}

/// The best common ancestor of two commits; `None` when they share no history.
pub fn merge_base(dir: &Path, a: &str, b: &str) -> Result<Option<String>> {
    if a == b {
        return Ok(Some(a.to_string()));
    }
    let out = super::command(dir, &["merge-base", a, b])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(Some(lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())),
        // Exit 1 without output: unrelated histories.
        Some(1) if out.stdout.is_empty() && out.stderr.is_empty() => Ok(None),
        _ => anyhow::bail!("git merge-base: {}", lossy(&out.stderr).trim()),
    }
}

/// Files changed between two commits.
pub fn between(dir: &Path, from: &str, to: &str) -> Result<Vec<FileChange>> {
    if from == to {
        return Ok(Vec::new());
    }
    let out = super::run(
        dir,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--numstat",
            "--no-renames",
            "--no-ext-diff",
            from,
            to,
            "--",
        ],
    )?;
    Ok(parse_numstat(&out))
}

/// Files in the worktree at `wt` (index + working tree) that differ from commit `from`,
/// limited to `paths` when given. Plumbing on purpose: `diff-index` never refreshes or writes the
/// index, so it can't take `index.lock` while an agent commits.
pub fn worktree(wt: &Path, from: &str, paths: Option<&[String]>) -> Result<Vec<FileChange>> {
    let mut args: Vec<String> = [
        "diff-index",
        "-z",
        "--numstat",
        "--no-renames",
        "--no-ext-diff",
        from,
        "--",
    ]
    .map(String::from)
    .to_vec();
    if let Some(paths) = paths {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        args.extend(paths.iter().map(|p| format!(":(literal){p}")));
    }
    let out = super::run(wt, &args)?;
    // Files whose stat info changed but content didn't come out as 0/0 (diff-index doesn't
    // refresh the index); they aren't changes.
    Ok(parse_numstat(&out)
        .into_iter()
        .filter(|f| f.binary || f.adds + f.dels > 0)
        .collect())
}

/// Branches that changed more files than this aren't checked for having landed in base.
pub const MAX_APPLIED_FILES: usize = 2000;

/// Whether every path in `paths` has the same content (or is absent) in commits `a` and `b`.
pub fn same_content(dir: &Path, a: &str, b: &str, paths: &[String]) -> Result<bool> {
    let mut args: Vec<String> = ["diff-tree", "-r", "--quiet", "--no-renames", "--no-ext-diff", a, b, "--"]
        .map(String::from)
        .to_vec();
    args.extend(paths.iter().map(|p| format!(":(literal){p}")));
    let out = super::command(dir, &args)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!("git diff-tree: {}", lossy(&out.stderr).trim()),
    }
}

/// Number of entries in a branch's reflog, counting at most `max` (0 when it has none, e.g.
/// `core.logAllRefUpdates` off).
pub fn reflog_len(dir: &Path, branch: &str, max: usize) -> Result<usize> {
    let out = super::run(
        dir,
        &[
            "log",
            "-g",
            "--format=%H",
            &format!("-n{max}"),
            &format!("refs/heads/{branch}"),
            "--",
        ],
    )?;
    Ok(out.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count())
}

/// `adds\tdels\tpath\0` records (`-\t-\t` for binary files), as written by `--numstat -z
/// --no-renames`.
pub fn parse_numstat(bytes: &[u8]) -> Vec<FileChange> {
    split_nul(bytes)
        .filter_map(|rec| {
            let rec = lossy(rec);
            let mut it = rec.splitn(3, '\t');
            let (a, d, path) = (it.next()?, it.next()?, it.next()?);
            if path.is_empty() {
                return None;
            }
            let binary = a == "-" || d == "-";
            Some(FileChange {
                path: path.to_string(),
                adds: a.parse().unwrap_or(0),
                dels: d.parse().unwrap_or(0),
                binary,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat() {
        let v = parse_numstat(b"3\t1\tsrc/a b.rs\0-\t-\timg.png\0\x30\t0\tsame\0");
        assert_eq!(
            v,
            [
                FileChange {
                    path: "src/a b.rs".into(),
                    adds: 3,
                    dels: 1,
                    binary: false
                },
                FileChange {
                    path: "img.png".into(),
                    adds: 0,
                    dels: 0,
                    binary: true
                },
                FileChange {
                    path: "same".into(),
                    adds: 0,
                    dels: 0,
                    binary: false
                },
            ]
        );
        assert!(parse_numstat(b"").is_empty());
    }
}
