//! Loading diffs for the diff view: a commit (`git show`) or a worktree's uncommitted changes
//! (`git diff HEAD` plus untracked files). Runs off the UI thread.

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

/// Untracked files shown with content; the rest are only listed.
const MAX_UNTRACKED_SHOWN: usize = 50;
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024;
const MAX_UNTRACKED_LINES: usize = 500;

const COMMON: &[&str] = &["--no-color", "--no-ext-diff", "--stat", "-p"];

pub fn load(target: &Target) -> Result<Vec<String>> {
    let text = match target {
        Target::Commit { dir, id } => {
            let mut args = vec!["show", "--no-show-signature", "--format=fuller"];
            args.extend(COMMON);
            args.extend([id.as_str(), "--"]);
            String::from_utf8_lossy(&git::run(dir, &args)?).into_owned()
        }
        Target::Worktree { path, has_head } => worktree_diff(path, *has_head)?,
    };
    Ok(sanitize(&text))
}

fn worktree_diff(wt: &Path, has_head: bool) -> Result<String> {
    let mut out = String::new();
    if has_head {
        let mut args = vec!["diff"];
        args.extend(COMMON);
        args.extend(["HEAD", "--"]);
        out.push_str(&String::from_utf8_lossy(&git::run(wt, &args)?));
    } else {
        // Unborn branch: nothing to diff against, show what is staged.
        let mut args = vec!["diff", "--cached"];
        args.extend(COMMON);
        out.push_str(&String::from_utf8_lossy(&git::run(wt, &args)?));
    }

    let raw = git::run(wt, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let untracked: Vec<String> = git::split_nul(&raw).map(git::lossy).collect();
    if !untracked.is_empty() {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        let _ = writeln!(out, "Untracked files ({}):", untracked.len());
        for f in &untracked {
            let _ = writeln!(out, "    {f}");
        }
        for f in untracked.iter().take(MAX_UNTRACKED_SHOWN) {
            out.push('\n');
            out.push_str(&untracked_as_diff(wt, f));
        }
        if untracked.len() > MAX_UNTRACKED_SHOWN {
            let _ = writeln!(
                out,
                "\n… {} more untracked files not shown",
                untracked.len() - MAX_UNTRACKED_SHOWN
            );
        }
    }
    if out.trim().is_empty() {
        out.push_str("No uncommitted changes.\n");
    }
    Ok(out)
}

/// Render an untracked file as a new-file diff, read directly from disk (no git process per file).
fn untracked_as_diff(wt: &Path, name: &str) -> String {
    let mut out = format!("diff --git a/{name} b/{name}\nnew file (untracked)\n");
    let path = wt.join(name);
    let mut buf = Vec::new();
    let size = match std::fs::File::open(&path).and_then(|f| {
        let size = f.metadata()?.len();
        f.take(MAX_UNTRACKED_BYTES).read_to_end(&mut buf)?;
        Ok(size)
    }) {
        Ok(s) => s,
        Err(e) => return out + &format!("(cannot read: {e})\n"),
    };
    if buf[..buf.len().min(8000)].contains(&0) {
        return out + &format!("Binary file ({size} bytes)\n");
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let _ = write!(out, "--- /dev/null\n+++ b/{name}\n@@ -0,0 +1,{} @@\n", lines.len());
    for l in lines.iter().take(MAX_UNTRACKED_LINES) {
        let _ = writeln!(out, "+{l}");
    }
    if lines.len() > MAX_UNTRACKED_LINES || size > MAX_UNTRACKED_BYTES {
        out.push_str("… (truncated)\n");
    }
    out
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

/// Index of the next (`forward`) or previous file header relative to `from`.
pub fn file_jump(lines: &[String], from: usize, forward: bool) -> Option<usize> {
    let is_header = |l: &String| l.starts_with("diff --git ") || l.starts_with("diff --cc ");
    if forward {
        (from + 1..lines.len()).find(|&i| is_header(&lines[i]))
    } else {
        (0..from).rev().find(|&i| is_header(&lines[i]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize("a\tb\r\n\x1b[31mx"), vec!["a   b", "�[31mx"]);
        assert_eq!(sanitize("ab\tc"), vec!["ab  c"]);
    }

    #[test]
    fn jumps_between_files() {
        let lines: Vec<String> = ["commit x", "diff --git a/a b/a", "+1", "diff --git a/b b/b", "+2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(file_jump(&lines, 0, true), Some(1));
        assert_eq!(file_jump(&lines, 1, true), Some(3));
        assert_eq!(file_jump(&lines, 3, true), None);
        assert_eq!(file_jump(&lines, 4, false), Some(3));
        assert_eq!(file_jump(&lines, 1, false), None);
    }
}
