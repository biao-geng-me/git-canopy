//! `git for-each-ref` for graph labels and change detection.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefKind {
    Branch,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefEntry {
    pub kind: RefKind,
    /// Short name: `main`, `origin/main`, `v1.0`.
    pub name: String,
    /// Commit the ref points at (annotated tags are peeled).
    pub target: String,
}

const FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(symref)";

/// Returns parsed refs plus the raw output (used as a cheap change fingerprint).
pub fn load(dir: &Path) -> Result<(Vec<RefEntry>, Vec<u8>)> {
    let raw = super::run(
        dir,
        &["for-each-ref", FORMAT, "refs/heads", "refs/remotes", "refs/tags"],
    )?;
    Ok((parse(&raw), raw))
}

pub fn parse(bytes: &[u8]) -> Vec<RefEntry> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\0');
            let (full, oid, peeled, symref) = (f.next()?, f.next()?, f.next()?, f.next()?);
            if !symref.is_empty() {
                return None; // e.g. origin/HEAD
            }
            const PREFIXES: [(&str, RefKind); 3] = [
                ("refs/heads/", RefKind::Branch),
                ("refs/remotes/", RefKind::Remote),
                ("refs/tags/", RefKind::Tag),
            ];
            let (kind, name) = PREFIXES
                .iter()
                .find_map(|&(prefix, kind)| Some((kind, full.strip_prefix(prefix)?)))?;
            let target = if peeled.is_empty() { oid } else { peeled };
            Some(RefEntry {
                kind,
                name: name.to_string(),
                target: target.to_string(),
            })
        })
        .collect()
}

/// Full names of the branches and remote-tracking branches (`refs/heads/…`, `refs/remotes/…`)
/// whose history contains `sha`; symbolic refs (`origin/HEAD`) are left out.
pub fn containing(dir: &Path, sha: &str) -> Result<Vec<String>> {
    let out = super::run(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(symref)",
            "--contains",
            sha,
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            let (name, symref) = l.split_once('\0')?;
            symref.is_empty().then(|| name.to_string())
        })
        .collect())
}

/// Where a branch's upstream is: `refs/remotes/origin/x` here, branch `x` of remote `origin`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// The remote-tracking ref, `refs/remotes/origin/x`.
    pub tracking: String,
    pub remote: String,
    /// The branch on the remote, `x` (without `refs/heads/`).
    pub branch: String,
}

impl Upstream {
    /// `origin/x`.
    pub fn short(&self) -> &str {
        short(&self.tracking)
    }
}

/// The upstream configured for local branch `branch` (it may no longer exist: "gone"). Only
/// upstreams on a remote, not another local branch.
pub fn upstream(dir: &Path, branch: &str) -> Result<Option<Upstream>> {
    let out = super::run(
        dir,
        &[
            "for-each-ref",
            "--format=%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut f = text.trim_end().split('\0');
    let (Some(tracking), Some(remote), Some(remote_ref)) = (f.next(), f.next(), f.next()) else {
        return Ok(None);
    };
    match (tracking.starts_with("refs/remotes/"), remote_ref.strip_prefix("refs/heads/")) {
        (true, Some(b)) if !remote.is_empty() && remote != "." => Ok(Some(Upstream {
            tracking: tracking.to_string(),
            remote: remote.to_string(),
            branch: b.to_string(),
        })),
        _ => Ok(None),
    }
}

/// The remote and branch a remote-tracking ref (`refs/remotes/origin/x`) stands for, by the
/// longest configured remote name it starts with (remote names may contain `/`).
pub fn remote_of(dir: &Path, tracking: &str) -> Result<Option<Upstream>> {
    let rest = tracking.strip_prefix("refs/remotes/").unwrap_or(tracking);
    let out = super::run(dir, &["remote"])?;
    let text = String::from_utf8_lossy(&out);
    let best = text
        .lines()
        .filter(|r| rest.strip_prefix(r).is_some_and(|b| b.starts_with('/')))
        .max_by_key(|r| r.len());
    Ok(best.map(|r| Upstream {
        tracking: format!("refs/remotes/{rest}"),
        remote: r.to_string(),
        branch: rest[r.len() + 1..].to_string(),
    }))
}

/// The commit `refname` points at, `None` when it doesn't exist.
pub fn resolve(dir: &Path, refname: &str) -> Option<String> {
    let out = super::run(dir, &["rev-parse", "--verify", "-q", &format!("{refname}^{{commit}}")]).ok()?;
    Some(String::from_utf8_lossy(&out).trim().to_string()).filter(|s| !s.is_empty())
}

/// `main` for `refs/heads/main`, `origin/main` for `refs/remotes/origin/main`.
pub fn short(refname: &str) -> &str {
    refname
        .strip_prefix("refs/heads/")
        .or_else(|| refname.strip_prefix("refs/remotes/"))
        .unwrap_or(refname)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kinds_peels_tags_skips_symrefs() {
        let raw = b"refs/heads/main\0aaa\0\0\n\
refs/remotes/origin/HEAD\0aaa\0\0refs/remotes/origin/main\n\
refs/remotes/origin/main\0bbb\0\0\n\
refs/tags/v0.1\0ttt\0ccc\0\n\
refs/tags/light\0ddd\0\0\n";
        let refs = parse(raw);
        assert_eq!(refs.len(), 4);
        assert_eq!(
            refs[0],
            RefEntry {
                kind: RefKind::Branch,
                name: "main".into(),
                target: "aaa".into()
            }
        );
        assert_eq!(refs[1].name, "origin/main");
        assert_eq!(refs[2].target, "ccc", "annotated tag is peeled");
        assert_eq!(refs[3].target, "ddd");
    }
}
