//! `git for-each-ref` for graph labels and change detection.

use std::path::Path;

use anyhow::Result;

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
        &[
            "for-each-ref",
            FORMAT,
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
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
