//! Loading diffs for the diff view: a commit (`git show`), a worktree's uncommitted changes
//! (`git diff-index HEAD` plus untracked files), or everything a worktree changed since it forked
//! from the base branch (`git diff-index <merge base>` plus untracked files), or what a worktree
//! and the others it overlaps with each did to their shared files, split into an overview and
//! one section per file. Runs off the UI thread.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::git;
use crate::model::Overlap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A commit, shown from the repository at `dir`.
    Commit { dir: PathBuf, id: String },
    /// Uncommitted changes of the worktree at `path`.
    Worktree { path: PathBuf, has_head: bool },
    /// Everything the worktree at `path` changed since its merge base with branch `base`:
    /// commits and uncommitted changes together.
    Branch { path: PathBuf, base: String },
    /// The files the worktree at `path` (named `name`) and other worktrees (or the base branch)
    /// both changed: per shared file, what each side did to it in its own work.
    Overlaps {
        path: PathBuf,
        name: String,
        overlaps: Vec<Overlap>,
    },
}

impl Target {
    /// The worktree whose working tree this diff reads (`None` for a commit).
    pub fn worktree(&self) -> Option<&Path> {
        match self {
            Target::Worktree { path, .. } | Target::Branch { path, .. } | Target::Overlaps { path, .. } => Some(path),
            Target::Commit { .. } => None,
        }
    }
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
    /// Shown after the counts in the file list (overlaps: the other side's name).
    pub note: Option<String>,
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
        Target::Worktree { .. } | Target::Branch { .. } => {
            let (wt, from) = worktree_base(target)?;
            let mut doc = worktree_diff(&wt, &from)?;
            if let Target::Branch { base, .. } = target {
                let short = &from[..from.len().min(10)];
                let mut intro = vec![
                    format!("Everything changed since branching from {base} at {short}: commits and uncommitted"),
                    "changes together, compared with the working tree.".into(),
                ];
                if doc.files.is_empty() {
                    doc.overview.clear();
                    intro.push(String::new());
                    intro.push(format!("No changes since branching from {base}."));
                } else if !doc.overview.is_empty() {
                    intro.push(String::new());
                }
                doc.overview.splice(0..0, intro);
            } else if doc.overview.is_empty() && doc.files.is_empty() {
                doc.overview.push("No uncommitted changes.".into());
            }
            Ok(doc)
        }
        Target::Overlaps { path, name, overlaps } => overlaps_doc(path, name, overlaps),
    }
}

/// One entry per other side and shared file: this worktree's own work on the file, then the
/// other side's.
fn overlaps_doc(path: &Path, name: &str, overlaps: &[Overlap]) -> Result<DiffDoc> {
    let mut doc = DiffDoc::default();
    if overlaps.is_empty() {
        doc.overview
            .push(format!("[{name}] no longer overlaps with other worktrees."));
        return Ok(doc);
    }
    doc.overview.extend([
        format!("Files [{name}] and other worktrees (or the base branch) both changed in their own"),
        "work, committed or not: likely merge conflicts, or two agents doing the same thing. Each".into(),
        "file shows what each side did to it, from where its own work began to its working tree.".into(),
    ]);
    for o in overlaps {
        let n = o.files.len();
        doc.overview.push(String::new());
        doc.overview.push(format!(
            "{} ({n} file{}):",
            side_label(o),
            if n == 1 { "" } else { "s" }
        ));
        doc.overview.extend(o.files.iter().map(|f| format!("    {f}")));

        let mine = parse(&side_patch(path, Some(path), &o.from, "HEAD", &o.files)?).files;
        let theirs = parse(&side_patch(
            path,
            o.path.as_deref(),
            &o.other_from,
            &o.other_head,
            &o.files,
        )?)
        .files;
        for file in &o.files {
            // An untracked directory (`dir/`) stands for the files under it.
            let of = |sections: &[FileDiff]| -> Vec<FileDiff> {
                sections
                    .iter()
                    .filter(|s| s.path == *file || (file.ends_with('/') && s.path.starts_with(file.as_str())))
                    .cloned()
                    .collect()
            };
            let (mine, theirs) = (of(&mine), of(&theirs));
            let mut lines = side_lines(&format!("{name} (this worktree)"), &o.from, &mine);
            lines.push(String::new());
            lines.extend(side_lines(&side_label(o), &o.other_from, &theirs));
            doc.files.push(FileDiff {
                path: file.clone(),
                status: match &mine[..] {
                    [one] => one.status,
                    _ => 'M',
                },
                adds: mine.iter().map(|f| f.adds).sum(),
                dels: mine.iter().map(|f| f.dels).sum(),
                lines,
                note: Some(format!("↔ {}", o.with)),
            });
        }
    }
    Ok(doc)
}

fn side_label(o: &Overlap) -> String {
    match o.path {
        Some(_) => o.with.clone(),
        None => format!("{} (base branch)", o.with),
    }
}

/// A side's part of an overlap entry: a `──` heading, then its diff sections.
fn side_lines(label: &str, from: &str, sections: &[FileDiff]) -> Vec<String> {
    let short = &from[..from.len().min(7)];
    let mut lines = vec![format!("── {label}, since {short} ──")];
    if sections.is_empty() {
        lines.push("(no difference any more)".into());
    }
    for s in sections {
        lines.extend(s.lines.iter().cloned());
    }
    lines
}

/// What one side of an overlap did to `files`, as raw patch text: from `from` to the working
/// tree at `wt` (untracked files included), or, for a side without a worktree (the base
/// branch), from `from` to `head` in the repository at `dir`.
fn side_patch(dir: &Path, wt: Option<&Path>, from: &str, head: &str, files: &[String]) -> Result<String> {
    if files.is_empty() {
        return Ok(String::new());
    }
    let specs: Vec<String> = files.iter().map(|f| format!(":(literal){f}")).collect();
    let mut args = match wt {
        Some(_) => vec![
            "diff-index",
            "-p",
            "--no-renames",
            "--no-color",
            "--no-ext-diff",
            from,
            "--",
        ],
        None => vec![
            "diff-tree",
            "-r",
            "-p",
            "--no-renames",
            "--no-color",
            "--no-ext-diff",
            from,
            head,
            "--",
        ],
    };
    args.extend(specs.iter().map(String::as_str));
    let mut out = String::from_utf8_lossy(&git::run(wt.unwrap_or(dir), &args)?).into_owned();
    if let Some(wt) = wt {
        let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
        args.extend(specs.iter().map(String::as_str));
        for name in git::split_nul(&git::run(wt, &args)?).map(git::lossy) {
            out.push_str(&untracked_patch(wt, &name));
        }
    }
    Ok(out)
}

/// The worktree and the commit a worktree target is compared against: HEAD (the empty tree on
/// an unborn branch), or the merge base with the base branch.
fn worktree_base(target: &Target) -> Result<(PathBuf, String)> {
    match target {
        Target::Worktree { path, has_head: true } => Ok((path.clone(), "HEAD".into())),
        Target::Worktree { path, has_head: false } => Ok((path.clone(), empty_tree(path)?)),
        Target::Branch { path, base } => {
            let mb = git::changes::merge_base(path, base, "HEAD")?
                .ok_or_else(|| anyhow::anyhow!("no history in common with {base}"))?;
            Ok((path.clone(), mb))
        }
        Target::Commit { .. } | Target::Overlaps { .. } => anyhow::bail!("not a worktree diff"),
    }
}

/// The empty tree's id (computed, not written).
fn empty_tree(dir: &Path) -> Result<String> {
    let out = git::run(dir, &["hash-object", "-t", "tree", "--stdin"])?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// Changes in the worktree `wt` relative to commit `from`, plus untracked files.
fn worktree_diff(wt: &Path, from: &str) -> Result<DiffDoc> {
    // Plumbing on purpose: porcelain `git diff` refreshes and rewrites the index (taking
    // index.lock) when it finds stat-dirty files, even with --no-optional-locks. `diff-index`
    // never writes, and skips files whose content is unchanged.
    let raw = git::run(wt, &["diff-index", "-p", "--stat", "-M", from, "--"])?;
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
                    note: None,
                }
            });
        }
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
        Target::Worktree { .. } | Target::Branch { .. } => {
            let (path, from) = worktree_base(target)?;
            let path = &path;
            let mut args = vec!["diff-index", "-p", "-M", "--no-color", "--no-ext-diff", &from, "--"];
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
        Target::Overlaps { path, name, overlaps } => {
            // Both sides' patches with a comment line before each: for pasting to an agent, not
            // for `git apply` (they're two versions of the same files).
            let mut out = String::new();
            for o in overlaps {
                let files: Vec<String> = match file {
                    Some(f) => o.files.iter().filter(|p| *p == f).cloned().collect(),
                    None => o.files.clone(),
                };
                if files.is_empty() {
                    continue;
                }
                let short = |c: &str| c[..c.len().min(7)].to_string();
                let _ = writeln!(out, "# {name} (this worktree), since {}", short(&o.from));
                out.push_str(&side_patch(path, Some(path), &o.from, "HEAD", &files)?);
                let _ = writeln!(out, "# {}, since {}", side_label(o), short(&o.other_from));
                out.push_str(&side_patch(
                    path,
                    o.path.as_deref(),
                    &o.other_from,
                    &o.other_head,
                    &files,
                )?);
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
        note: None,
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
        note: None,
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
