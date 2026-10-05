//! Loading diffs for the diff view: a commit (`git show`) or a worktree's uncommitted changes
//! (`git diff-index HEAD` plus untracked files), split into an overview and one section per file.
//! Runs off the UI thread.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::git;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A commit, shown from the repository at `dir`.
    Commit { dir: PathBuf, id: String },
    /// Uncommitted changes of the worktree at `path`.
    Worktree { path: PathBuf, has_head: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    /// `M`odified, `A`dded, `D`eleted, `R`enamed, `C`opied, `U`nmerged (combined diff), `?` untracked.
    pub status: char,
    pub adds: usize,
    pub dels: usize,
    /// The file's section, starting with its `diff --git` header.
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffDoc {
    /// Everything before the first file: commit header and message, diffstat, untracked list.
    pub overview: Vec<String>,
    pub files: Vec<FileDiff>,
}

/// Untracked files shown with content; the rest are only listed.
const MAX_UNTRACKED_SHOWN: usize = 50;
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024;
const MAX_UNTRACKED_LINES: usize = 500;

pub fn load(target: &Target) -> Result<DiffDoc> {
    match target {
        Target::Commit { dir, id } => {
            let args = [
                "show",
                "--no-show-signature",
                "--format=fuller",
                "--no-color",
                "--no-ext-diff",
                "--stat",
                "-p",
                id,
                "--",
            ];
            Ok(parse(&String::from_utf8_lossy(&git::run(dir, &args)?)))
        }
        Target::Worktree { path, has_head } => worktree_diff(path, *has_head),
    }
}

fn worktree_diff(wt: &Path, has_head: bool) -> Result<DiffDoc> {
    // Plumbing on purpose: porcelain `git diff` refreshes and rewrites the index (taking
    // index.lock) when it finds stat-dirty files, even with --no-optional-locks. `diff-index`
    // never writes, and skips files whose content is unchanged.
    let base = if has_head {
        "HEAD".to_string()
    } else {
        // Unborn branch: compare against the empty tree (computed, not written).
        let out = git::run(wt, &["hash-object", "-t", "tree", "--stdin"])?;
        String::from_utf8_lossy(&out).trim().to_string()
    };
    let raw = git::run(wt, &["diff-index", "-p", "--stat", "-M", &base, "--"])?;
    let mut doc = parse(&String::from_utf8_lossy(&raw));
    while doc.overview.last().is_some_and(|l| l.is_empty()) {
        doc.overview.pop();
    }

    let raw = git::run(wt, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let untracked: Vec<String> = git::split_nul(&raw).map(git::lossy).collect();
    if !untracked.is_empty() {
        if !doc.overview.is_empty() {
            doc.overview.push(String::new());
        }
        doc.overview.push(format!("Untracked files ({}):", untracked.len()));
        doc.overview.extend(untracked.iter().map(|f| format!("    {f}")));
        for (i, f) in untracked.iter().enumerate() {
            doc.files.push(if i < MAX_UNTRACKED_SHOWN {
                untracked_file(wt, f)
            } else {
                FileDiff {
                    path: f.clone(),
                    status: '?',
                    adds: 0,
                    dels: 0,
                    lines: vec![format!(
                        "(content not shown: more than {MAX_UNTRACKED_SHOWN} untracked files)"
                    )],
                }
            });
        }
    }
    if doc.overview.is_empty() && doc.files.is_empty() {
        doc.overview.push("No uncommitted changes.".into());
    }
    Ok(doc)
}

/// Untracked files bigger than this are left out of a copied patch.
const MAX_PATCH_UNTRACKED_BYTES: u64 = 1024 * 1024;

/// Raw diff text of `target` for copying, limited to `file` if given: `git show` output for a
/// commit; for a worktree the `diff-index` patch plus untracked files as new-file sections, in a
/// form `git apply` accepts. Unlike [`load`]'s lines, nothing is sanitized or truncated.
pub fn patch(target: &Target, file: Option<&str>) -> Result<String> {
    let spec = file.map(|f| format!(":(literal){f}"));
    let spec = spec.as_deref();
    match target {
        Target::Commit { dir, id } => {
            // A single file without the commit header, so it pastes as a plain patch.
            let format = if file.is_some() { "--format=" } else { "--format=medium" };
            let mut args = vec![
                "show",
                "--no-show-signature",
                "--no-color",
                "--no-ext-diff",
                format,
                "-p",
                id,
                "--",
            ];
            args.extend(spec);
            Ok(String::from_utf8_lossy(&git::run(dir, &args)?).into_owned())
        }
        Target::Worktree { path, has_head } => {
            let base = if *has_head {
                "HEAD".to_string()
            } else {
                let out = git::run(path, &["hash-object", "-t", "tree", "--stdin"])?;
                String::from_utf8_lossy(&out).trim().to_string()
            };
            let mut args = vec!["diff-index", "-p", "-M", "--no-color", "--no-ext-diff", &base, "--"];
            args.extend(spec);
            let mut out = String::from_utf8_lossy(&git::run(path, &args)?).into_owned();
            let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
            args.extend(spec);
            let raw = git::run(path, &args)?;
            for name in git::split_nul(&raw).map(git::lossy) {
                out.push_str(&untracked_patch(path, &name));
            }
            Ok(out)
        }
    }
}

/// An untracked file as a `git apply`-able new-file section (binary and huge files only noted).
fn untracked_patch(wt: &Path, name: &str) -> String {
    let mut out = format!("diff --git a/{name} b/{name}\nnew file mode 100644\n");
    let read = std::fs::File::open(wt.join(name)).and_then(|f| {
        let size = f.metadata()?.len();
        let mut buf = Vec::new();
        f.take(MAX_PATCH_UNTRACKED_BYTES + 1).read_to_end(&mut buf)?;
        Ok((size, buf))
    });
    let text = match read {
        Ok((size, _)) if size > MAX_PATCH_UNTRACKED_BYTES => Err(format!("{size} bytes, not included")),
        Ok((_, buf)) => match String::from_utf8(buf) {
            Ok(t) if !t.contains('\0') => Ok(t),
            _ => Err("binary, not included".into()),
        },
        Err(e) => Err(format!("cannot read: {e}")),
    };
    match text {
        Ok(t) if t.is_empty() => {}
        Ok(t) => {
            // Lines keep their own endings (CRLF files stay CRLF, so the patch still applies).
            let lines: Vec<&str> = t.split_inclusive('\n').collect();
            let _ = write!(out, "--- /dev/null\n+++ b/{name}\n@@ -0,0 +1,{} @@\n", lines.len());
            for l in lines {
                out.push('+');
                out.push_str(l);
            }
            if !t.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
        Err(note) => {
            let _ = writeln!(out, "# untracked file ({note})");
        }
    }
    out
}

/// An untracked file as a new-file section, read directly from disk (no git process per file).
fn untracked_file(wt: &Path, name: &str) -> FileDiff {
    let mut out = format!("diff --git a/{name} b/{name}\nnew file (untracked)\n");
    let mut adds = 0;
    let mut buf = Vec::new();
    let read = std::fs::File::open(wt.join(name)).and_then(|f| {
        let size = f.metadata()?.len();
        f.take(MAX_UNTRACKED_BYTES).read_to_end(&mut buf)?;
        Ok(size)
    });
    match read {
        Err(e) => {
            let _ = writeln!(out, "(cannot read: {e})");
        }
        Ok(size) if buf[..buf.len().min(8000)].contains(&0) => {
            let _ = writeln!(out, "Binary file ({size} bytes)");
        }
        Ok(size) => {
            let text = String::from_utf8_lossy(&buf);
            let lines: Vec<&str> = text.lines().collect();
            let _ = write!(out, "--- /dev/null\n+++ b/{name}\n@@ -0,0 +1,{} @@\n", lines.len());
            for l in lines.iter().take(MAX_UNTRACKED_LINES) {
                let _ = writeln!(out, "+{l}");
            }
            adds = lines.len();
            if lines.len() > MAX_UNTRACKED_LINES || size > MAX_UNTRACKED_BYTES {
                out.push_str("… (truncated)\n");
            }
        }
    }
    FileDiff {
        path: name.to_string(),
        status: '?',
        adds,
        dels: 0,
        lines: sanitize(&out),
    }
}

fn is_file_header(line: &str) -> bool {
    line.starts_with("diff --git ") || line.starts_with("diff --cc ") || line.starts_with("diff --combined ")
}

/// Split `git show` / `git diff` output into the overview and per-file sections.
pub fn parse(text: &str) -> DiffDoc {
    let mut doc = DiffDoc::default();
    let mut current: Option<Vec<String>> = None;
    for line in sanitize(text) {
        if is_file_header(&line) {
            if let Some(lines) = current.take() {
                doc.files.push(file_section(lines));
            }
            current = Some(vec![line]);
        } else if let Some(lines) = current.as_mut() {
            lines.push(line);
        } else {
            doc.overview.push(line);
        }
    }
    if let Some(lines) = current {
        doc.files.push(file_section(lines));
    }
    doc
}

fn file_section(lines: Vec<String>) -> FileDiff {
    let header = &lines[0];
    let combined = !header.starts_with("diff --git ");
    let mut status = if combined { 'U' } else { 'M' };
    let mut path: Option<String> = None;
    let mut old_path: Option<String> = None;
    let (mut adds, mut dels) = (0, 0);
    let mut in_hunks = false;
    for l in &lines[1..] {
        if in_hunks {
            match l.as_bytes().first() {
                Some(b'+') => adds += 1,
                Some(b'-') => dels += 1,
                _ => {}
            }
            continue;
        }
        if l.starts_with("@@") {
            in_hunks = true;
        } else if l.starts_with("new file") {
            status = 'A';
        } else if l.starts_with("deleted file") {
            status = 'D';
        } else if let Some(p) = l.strip_prefix("rename to ") {
            status = 'R';
            path = Some(p.to_string());
        } else if let Some(p) = l.strip_prefix("copy to ") {
            status = 'C';
            path = Some(p.to_string());
        } else if let Some(p) = l.strip_prefix("+++ ") {
            if p != "/dev/null" && path.is_none() {
                path = Some(p.strip_prefix("b/").unwrap_or(p).to_string());
            }
        } else if let Some(p) = l.strip_prefix("--- ") {
            old_path = Some(p.strip_prefix("a/").unwrap_or(p).to_string());
        }
    }
    let path = path
        .or_else(|| old_path.filter(|p| p != "/dev/null"))
        .unwrap_or_else(|| header_path(header));
    FileDiff {
        path,
        status,
        adds,
        dels,
        lines,
    }
}

/// Path from a `diff --git a/P b/P` (or `diff --cc P`) header, used when there are no `+++`
/// lines (binary files, mode-only changes).
fn header_path(header: &str) -> String {
    if let Some(p) = header
        .strip_prefix("diff --cc ")
        .or_else(|| header.strip_prefix("diff --combined "))
    {
        return p.to_string();
    }
    let rest = header.strip_prefix("diff --git ").unwrap_or(header);
    // Unrenamed: "a/P b/P", so both halves are equal.
    let n = rest.len();
    if n >= 5 && (n - 5).is_multiple_of(2) {
        let half = (n - 5) / 2;
        if let (Some(a), Some(b)) = (rest.get(2..2 + half), rest.get(half + 5..))
            && a == b
        {
            return a.to_string();
        }
    }
    rest.to_string()
}

/// Expand tabs and replace control characters so every line renders predictably.
pub fn sanitize(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| {
            let mut out = String::with_capacity(line.len());
            let mut col = 0;
            for c in line.chars() {
                match c {
                    '\t' => {
                        let n = 4 - col % 4;
                        out.extend(std::iter::repeat_n(' ', n));
                        col += n;
                    }
                    '\r' => {}
                    c if c.is_control() => {
                        out.push('�');
                        col += 1;
                    }
                    c => {
                        out.push(c);
                        col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    }
                }
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize("a\tb\r\n\x1b[31mx"), vec!["a   b", "�[31mx"]);
        assert_eq!(sanitize("ab\tc"), vec!["ab  c"]);
    }

    const SHOW: &str = "commit abc
Author: A

    subject
---
 a.rs | 2 +-
 2 files changed

diff --git a/a.rs b/a.rs
index 1..2 100644
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,2 @@
-old
+new
+++not a header
 ctx
diff --git a/old name.txt b/new name.txt
similarity index 90%
rename from old name.txt
rename to new name.txt
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/img.png b/img.png
new file mode 100644
Binary files /dev/null and b/img.png differ
diff --cc conflict.rs
index 1,2..3
@@@ -1,1 -1,1 +1,1 @@@
";

    #[test]
    fn splits_files() {
        let doc = parse(SHOW);
        assert_eq!(doc.overview[0], "commit abc");
        assert_eq!(doc.overview.last().unwrap(), "");
        let summary: Vec<_> = doc
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status, f.adds, f.dels))
            .collect();
        assert_eq!(
            summary,
            [
                ("a.rs", 'M', 2, 1),
                ("new name.txt", 'R', 0, 0),
                ("gone.txt", 'D', 0, 1),
                ("img.png", 'A', 0, 0),
                ("conflict.rs", 'U', 0, 0),
            ]
        );
        assert_eq!(doc.files[0].lines[0], "diff --git a/a.rs b/a.rs");
        assert_eq!(doc.files[0].lines.len(), 9);
    }

    #[test]
    fn header_paths() {
        assert_eq!(header_path("diff --git a/x y.rs b/x y.rs"), "x y.rs");
        assert_eq!(header_path("diff --cc m.rs"), "m.rs");
        assert_eq!(header_path("diff --git a/a b/b"), "a/a b/b");
    }
}
