//! Commit history for the graph: `git log --topo-order -z` with a fixed field layout.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{lossy, split_nul};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    /// Author time, unix seconds.
    pub time: i64,
    pub subject: String,
}

/// Fields per commit in [`FORMAT`]; with `-z` both fields and commits are NUL-terminated.
const FIELDS: usize = 5;
const FORMAT: &str = "--format=%H%x00%P%x00%an%x00%at%x00%s";

/// Load up to `limit` commits reachable from branches, remotes, tags and the given extra
/// revisions (worktree HEADs, which may be detached). Stash and notes refs are deliberately
/// excluded: they are noise in an overview.
pub fn load(dir: &Path, limit: usize, heads: &[String]) -> Result<Vec<Commit>> {
    let mut args: Vec<String> = [
        "log",
        "--topo-order",
        "-z",
        "--no-show-signature",
        "--no-color",
        FORMAT,
        "--branches",
        "--remotes",
        "--tags",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("--max-count={limit}"));
    args.extend(heads.iter().cloned());
    args.push("--".into());
    let raw = super::run(dir, &args)?;
    Ok(parse(&raw))
}

/// Commit time and subject of exactly the given commits (no history walk), in one process.
pub fn describe(dir: &Path, ids: &[String]) -> Result<Vec<(String, i64, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<String> = [
        "log",
        "--no-walk=unsorted",
        "-z",
        "--no-show-signature",
        "--format=%H%x00%ct%x00%s",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(ids.iter().cloned());
    args.push("--".into());
    let raw = super::run(dir, &args)?;
    let recs: Vec<&[u8]> = split_nul(&raw).collect();
    Ok(recs
        .as_chunks::<3>()
        .0
        .iter()
        .map(|f| {
            (
                lossy(f[0]).trim().to_string(),
                lossy(f[1]).trim().parse().unwrap_or(0),
                lossy(f[2]),
            )
        })
        .collect())
}

pub fn parse(bytes: &[u8]) -> Vec<Commit> {
    let recs: Vec<&[u8]> = split_nul(bytes).collect();
    recs.as_chunks::<FIELDS>()
        .0
        .iter()
        .map(|f| {
            // Records after the first start with the newline git puts between commits.
            let id = lossy(f[0]).trim().to_string();
            Commit {
                id,
                parents: lossy(f[1]).split_whitespace().map(str::to_string).collect(),
                author: lossy(f[2]),
                time: lossy(f[3]).trim().parse().unwrap_or(0),
                subject: lossy(f[4]),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commits() {
        let raw = b"aaa\0bbb ccc\0Ann\x001700000000\0Merge thing\0\
bbb\0ddd\0Bob\x001699990000\0fix: stuff\0\
ddd\0\0Bob\x001699980000\0initial\0";
        let cs = parse(raw);
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].parents, vec!["bbb", "ccc"]);
        assert_eq!(cs[1].author, "Bob");
        assert_eq!(cs[1].time, 1699990000);
        assert!(cs[2].parents.is_empty());
        assert_eq!(cs[2].subject, "initial");
    }

    #[test]
    fn tolerates_newline_separators_and_empty() {
        let cs = parse(b"aaa\0\0A\x001\0s\0\nbbb\0\0B\x002\0t\0");
        assert_eq!(cs[1].id, "bbb");
        assert!(parse(b"").is_empty());
    }
}
