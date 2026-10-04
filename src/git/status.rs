//! `git status --porcelain=v2 --branch -z`.

use std::path::Path;

use anyhow::Result;

use super::{lossy, non_null_oid, split_nul};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// HEAD commit, `None` on an unborn branch.
    pub oid: Option<String>,
    /// Branch name, `None` when detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    /// (ahead, behind) relative to upstream.
    pub upstream_ab: Option<(u32, u32)>,
    /// Paths with changes in the index.
    pub staged: u32,
    /// Paths with changes in the working tree not yet staged.
    pub modified: u32,
    pub untracked: u32,
    pub unmerged: u32,
    /// Changed paths (first [`MAX_PATHS`]), relative to the worktree root.
    pub paths: Vec<String>,
}

/// Cap on [`Status::paths`]; counts stay exact beyond it.
pub const MAX_PATHS: usize = 256;

impl Status {
    pub fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.untracked + self.unmerged > 0
    }
}

pub fn load(wt: &Path) -> Result<Status> {
    let raw = super::run(wt, &["status", "--porcelain=v2", "--branch", "-z"])?;
    Ok(parse(&raw))
}

pub fn parse(bytes: &[u8]) -> Status {
    let mut st = Status::default();
    let mut recs = split_nul(bytes);
    while let Some(rec) = recs.next() {
        let Some((&kind, rest)) = rec.split_first() else {
            continue;
        };
        match kind {
            b'#' => parse_header(&mut st, &lossy(rest)),
            b'1' | b'2' | b'u' | b'?' if st.paths.len() < MAX_PATHS => {
                // Fixed field counts before the path: 1 → 8, 2 → 9, u → 10, ? → 1.
                let fields = match kind {
                    b'1' => 9,
                    b'2' => 10,
                    b'u' => 11,
                    _ => 2,
                };
                if let Some(p) = lossy(rec).splitn(fields, ' ').nth(fields - 1) {
                    st.paths.push(p.to_string());
                }
                count(&mut st, kind, rest, &mut recs);
            }
            _ => count(&mut st, kind, rest, &mut recs),
        }
    }
    st
}

fn count<'a>(st: &mut Status, kind: u8, rest: &[u8], recs: &mut impl Iterator<Item = &'a [u8]>) {
    match kind {
        b'1' | b'2' => {
            let xy = rest.get(1..3).unwrap_or(b"..");
            if xy[0] != b'.' {
                st.staged += 1;
            }
            if xy[1] != b'.' {
                st.modified += 1;
            }
            if kind == b'2' {
                // Renames/copies carry the original path as a separate record.
                recs.next();
            }
        }
        b'u' => st.unmerged += 1,
        b'?' => st.untracked += 1,
        _ => {}
    }
}

fn parse_header(st: &mut Status, line: &str) {
    let line = line.trim_start();
    let Some((key, val)) = line.split_once(' ') else {
        return;
    };
    match key {
        "branch.oid" => st.oid = non_null_oid(val).filter(|v| v != "(initial)"),
        "branch.head" => st.branch = (val != "(detached)").then(|| val.to_string()),
        "branch.upstream" => st.upstream = Some(val.to_string()),
        "branch.ab" => {
            let mut it = val.split_whitespace();
            let a = it.next().and_then(|s| s.strip_prefix('+')?.parse().ok());
            let b = it.next().and_then(|s| s.strip_prefix('-')?.parse().ok());
            if let (Some(a), Some(b)) = (a, b) {
                st.upstream_ab = Some((a, b));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers_and_entries() {
        let raw = b"# branch.oid 35d6da7ad1b7d428705681a0acc83773329b029b\0# branch.head feat-auth\0\
# branch.upstream origin/feat-auth\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 044e 044e src/auth.rs\0\
1 M. N... 100644 100644 100644 f328 341b src/main.rs\0\
1 MM N... 100644 100644 100644 f328 341b src/both.rs\0\
2 R. N... 100644 100644 100644 aaaa aaaa R100 new name.rs\0old name.rs\0\
u UU N... 100644 100644 100644 100644 a b c README.md\0\
? notes.md\0? docs/\0";
        let st = parse(raw);
        assert_eq!(st.branch.as_deref(), Some("feat-auth"));
        assert_eq!(st.upstream.as_deref(), Some("origin/feat-auth"));
        assert_eq!(st.upstream_ab, Some((2, 1)));
        assert_eq!((st.staged, st.modified, st.untracked, st.unmerged), (3, 2, 2, 1));
        assert!(st.is_dirty());
        assert_eq!(
            st.paths,
            [
                "src/auth.rs",
                "src/main.rs",
                "src/both.rs",
                "new name.rs",
                "README.md",
                "notes.md",
                "docs/"
            ]
        );
    }

    #[test]
    fn detached_initial_clean() {
        let st = parse(b"# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!(st.oid, None);
        assert_eq!(st.branch, None);
        assert!(!st.is_dirty());
    }

    #[test]
    fn rename_orig_path_not_counted_as_entry() {
        // An original path that looks like an untracked record must be skipped.
        let st = parse(b"2 R. N... 100644 100644 100644 a a R100 b\0? weird\0");
        assert_eq!((st.staged, st.untracked), (1, 0));
    }
}
