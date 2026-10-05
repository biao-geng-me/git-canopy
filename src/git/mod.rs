//! Git layer: runs the `git` executable and parses its machine-readable output.
//!
//! Every invocation goes through [`command`], which disables optional locks so that reading status
//! never takes `index.lock` while an agent is committing. Nothing here writes to the repository.

pub mod changes;
pub mod log;
pub mod refs;
pub mod status;
pub mod worktree;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Environment variables that would override `-C <dir>` repository discovery.
const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_NAMESPACE",
];

/// Build a `git` command for `dir` with optional locks off and no prompts or pagers.
pub fn command<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks")
        .arg("--no-pager")
        .args(["-c", "core.quotepath=off", "-c", "color.ui=never"])
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    for var in REPO_ENV {
        cmd.env_remove(var);
    }
    cmd
}

/// Run git in `dir` and return stdout; a non-zero exit becomes an error carrying stderr.
pub fn run<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Vec<u8>> {
    let out = command(dir, args)
        .output()
        .context("failed to run git (is it installed and on PATH?)")?;
    if !out.status.success() {
        let sub = args
            .iter()
            .map(|a| a.as_ref().to_string_lossy())
            .find(|a| !a.starts_with('-'))
            .unwrap_or_default();
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {sub}: {}", err.trim());
    }
    Ok(out.stdout)
}

/// Where a repository lives, resolved from any path inside it.
#[derive(Clone, Debug)]
pub struct RepoPaths {
    /// The shared git dir (`.git` of the main worktree, or the bare repo).
    pub common_dir: PathBuf,
    /// Top level of the worktree containing the start path, if it is inside one.
    pub toplevel: Option<PathBuf>,
}

/// Resolve the repository containing `path`; errors if it is not inside a git repository.
pub fn discover(path: &Path) -> Result<RepoPaths> {
    let out = run(path, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map_err(|e| {
        // Plain message for the common case; keep git's words for anything else (e.g. safe.directory).
        if e.to_string().contains("not a git repository") {
            anyhow::anyhow!("not a git repository: {}", path.display())
        } else {
            e.context(format!("cannot open repository at {}", path.display()))
        }
    })?;
    let common_dir = PathBuf::from(trim_line(&out));
    let toplevel = run(path, &["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .ok()
        .map(|o| PathBuf::from(trim_line(&o)))
        .filter(|p| !p.as_os_str().is_empty());
    Ok(RepoPaths {
        common_dir: canonical(&common_dir),
        toplevel: toplevel.map(|p| canonical(&p)),
    })
}

/// Commits in `left` but not `right`, and in `right` but not `left`.
pub fn left_right_count(dir: &Path, left: &str, right: &str) -> Result<(u32, u32)> {
    let range = format!("{left}...{right}");
    let out = run(dir, &["rev-list", "--left-right", "--count", &range, "--"])?;
    parse_left_right(&out).with_context(|| format!("unexpected rev-list output for {range}"))
}

pub fn parse_left_right(out: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(out).ok()?;
    let mut it = text.split_whitespace().map(|n| n.parse::<u32>());
    match (it.next(), it.next()) {
        (Some(Ok(l)), Some(Ok(r))) => Some((l, r)),
        _ => None,
    }
}

fn trim_line(out: &[u8]) -> String {
    String::from_utf8_lossy(out).trim_end_matches(['\n', '\r']).to_string()
}

/// Canonicalize if possible, otherwise keep the path as given.
pub fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Split NUL-separated output into records, dropping the trailing empty one.
pub(crate) fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    bytes.split(|&b| b == 0).filter(move |_| !bytes.is_empty())
}

pub(crate) fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// An all-zero object id (unborn branch) means "no commit".
pub(crate) fn non_null_oid(s: &str) -> Option<String> {
    if s.is_empty() || s.bytes().all(|b| b == b'0') {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_right() {
        assert_eq!(parse_left_right(b"3\t7\n"), Some((3, 7)));
        assert_eq!(parse_left_right(b"oops"), None);
    }

    #[test]
    fn nul_split() {
        let v: Vec<&[u8]> = split_nul(b"a\0b\0\0c\0").collect();
        assert_eq!(v, vec![&b"a"[..], b"b", b"", b"c"]);
        assert_eq!(split_nul(b"").count(), 0);
    }

    #[test]
    fn null_oid() {
        assert_eq!(non_null_oid("0000000000"), None);
        assert_eq!(non_null_oid("abc"), Some("abc".into()));
    }
}
