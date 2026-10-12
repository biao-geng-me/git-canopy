//! Removing a worktree whose work is in the base branch: its checkout, its branch and the herdr
//! workspace backed by it. canopy's only write to a repository, made only after the user confirms
//! a [`Plan`], and only after everything is checked again right before acting ([`run`]).
//!
//! Nothing is forced: `git worktree remove` (or herdr's `worktree.remove`, which runs it) refuses
//! a dirty checkout by itself, and the branch is deleted only while it still points at the commit
//! that was verified to be merged.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git::{self, changes};
use crate::herdr::{self, AgentState};
use crate::model::{Landed, WorktreeView};

/// Ignored entries listed in a plan (the rest is counted).
const MAX_IGNORED: usize = 200;

/// What removing one worktree will do, for the confirm popup and for [`run`].
#[derive(Clone, Debug)]
pub struct Plan {
    pub common_dir: PathBuf,
    pub path: PathBuf,
    pub name: String,
    /// Deleted after the checkout, if it still points at `head`.
    pub branch: Option<String>,
    pub head: String,
    pub base: String,
    pub landed: Landed,
    /// herdr socket, when herdr is in use and reachable.
    pub herdr: Option<PathBuf>,
    /// The herdr workspace backed by this checkout: herdr removes the checkout and closes it.
    pub workspace: Option<herdr::Workspace>,
    /// Idle or done agents in this worktree outside that workspace: their panes stay open, in a
    /// directory that no longer exists.
    pub stray_agents: Vec<String>,
    /// Ignored files and directories that go with the checkout (first [`MAX_IGNORED`]).
    pub ignored: Vec<String>,
    pub ignored_total: usize,
}

/// Check `w` again and work out what removing it involves. `herdr`: the socket when herdr is in
/// use; `own_workspace`: the herdr workspace canopy itself runs in (never closed).
pub fn plan(
    common_dir: &Path,
    w: &WorktreeView,
    base: &str,
    herdr: Option<PathBuf>,
    own_workspace: Option<&str>,
) -> Result<Plan> {
    let head = w.head.clone().context("no HEAD")?;
    let landed = verify(common_dir, &w.path, &head, w.branch.as_deref(), base)?;
    let view = herdr.as_deref().and_then(|p| herdr::read_view_at(p).ok());
    let herdr = herdr.filter(|_| view.is_some());
    let (workspace, stray_agents) = match &view {
        Some(view) => herdr_part(view, &w.path, own_workspace)?,
        None => (None, Vec::new()),
    };
    let (ignored, ignored_total) = ignored(&w.path);
    Ok(Plan {
        common_dir: common_dir.to_path_buf(),
        path: w.path.clone(),
        name: w.name.clone(),
        branch: w.branch.clone(),
        head,
        base: base.to_string(),
        landed,
        herdr,
        workspace,
        stray_agents,
        ignored,
        ignored_total,
    })
}

/// Carry out `plan` after checking it once more. Returns the notice for the footer.
pub fn run(plan: &Plan, own_workspace: Option<&str>) -> Result<String> {
    verify(&plan.common_dir, &plan.path, &plan.head, plan.branch.as_deref(), &plan.base)?;
    let workspace = match &plan.herdr {
        Some(sock) => {
            let view = herdr::read_view_at(sock).context("herdr didn't answer")?;
            herdr_part(&view, &plan.path, own_workspace)?.0
        }
        None => None,
    };
    let path = plan.path.to_string_lossy().into_owned();
    match (&workspace, &plan.herdr) {
        (Some(ws), Some(sock)) => herdr::remove_worktree_at(sock, &ws.id).context("herdr")?,
        _ => {
            git::run(&plan.common_dir, &["worktree", "remove", &path])?;
        }
    }
    let short = &plan.head[..plan.head.len().min(7)];
    let mut notice = format!("removed {}", plan.name);
    if let Some(ws) = &workspace {
        notice.push_str(&format!(" and herdr workspace {}", ws.label));
    }
    if let Some(b) = &plan.branch {
        // Only while it still points at the verified commit.
        let at = git::run(&plan.common_dir, &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")]);
        if at.is_ok_and(|o| String::from_utf8_lossy(&o).trim() == plan.head) {
            git::run(&plan.common_dir, &["branch", "-D", b])?;
            notice.push_str(&format!(" · branch {b} was {short} (git branch {b} {short} restores it)"));
        } else {
            notice.push_str(&format!(" · kept branch {b}: it moved"));
        }
    }
    Ok(notice)
}

/// Check that the worktree at `path` is still clean, on `head` (and `branch`), and that its work
/// is in `base`.
fn verify(common_dir: &Path, path: &Path, head: &str, branch: Option<&str>, base: &str) -> Result<Landed> {
    let st = git::status::load(path)?;
    if st.staged + st.modified + st.unmerged > 0 {
        bail!("it has uncommitted changes");
    }
    if st.untracked > 0 {
        bail!("it has {} untracked file(s)", st.untracked);
    }
    if st.oid.as_deref() != Some(head) || st.branch.as_deref() != branch {
        bail!("its HEAD moved");
    }
    let base_sha = git::run(common_dir, &["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .with_context(|| format!("no base branch {base}"))?;
    match landed(common_dir, &base_sha, head, branch)? {
        Some(l @ (Landed::Merged | Landed::Applied)) => Ok(l),
        _ => bail!("its work isn't in {base}"),
    }
}

/// Whether `head`'s work is in the base branch (at `base_sha`); see `model::Landed`.
pub fn landed(dir: &Path, base_sha: &str, head: &str, branch: Option<&str>) -> Result<Option<Landed>> {
    let (_, ahead) = git::left_right_count(dir, base_sha, head)?;
    if ahead == 0 {
        let Some(b) = branch else { return Ok(None) };
        return Ok(match changes::reflog_len(dir, b, 2)? {
            n if n >= 2 => Some(Landed::Merged),
            1 => Some(Landed::Unused),
            _ => None,
        });
    }
    let Some(fork) = changes::merge_base(dir, base_sha, head)? else {
        return Ok(None);
    };
    let files: Vec<String> = changes::between(dir, &fork, head)?.into_iter().map(|f| f.path).collect();
    if files.is_empty() || files.len() > changes::MAX_APPLIED_FILES {
        return Ok(None);
    }
    Ok(changes::same_content(dir, head, base_sha, &files)?.then_some(Landed::Applied))
}

/// The herdr workspace backed by the checkout at `path`, and the other agents in it. Fails when
/// an agent there is busy, or when that workspace is canopy's own.
fn herdr_part(view: &herdr::View, path: &Path, own_workspace: Option<&str>) -> Result<(Option<herdr::Workspace>, Vec<String>)> {
    let ws = view.workspaces.iter().find(|w| w.checkout.as_deref() == Some(path)).cloned();
    if let (Some(ws), Some(own)) = (&ws, own_workspace)
        && ws.id == own
    {
        bail!("canopy runs in its herdr workspace {}", ws.label);
    }
    let mut stray = Vec::new();
    for a in &view.agents {
        let in_ws = ws.as_ref().is_some_and(|w| w.id == a.workspace_id);
        if !in_ws && !a.cwd.as_deref().is_some_and(|c| c.starts_with(path)) {
            continue;
        }
        if !matches!(a.state, AgentState::Idle | AgentState::Done) {
            bail!("agent {} is {} in it", a.label, a.state.word());
        }
        if !in_ws {
            stray.push(format!("{} ({}, pane {})", a.label, a.state.word(), a.pane_id));
        }
    }
    Ok((ws, stray))
}

/// Ignored files and directories (`dir/`) in the checkout, which removing it deletes too.
fn ignored(path: &Path) -> (Vec<String>, usize) {
    let args = ["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"];
    let Ok(out) = git::run(path, &args) else {
        return (Vec::new(), 0);
    };
    let all: Vec<String> = git::split_nul(&out).map(git::lossy).collect();
    let total = all.len();
    (all.into_iter().take(MAX_IGNORED).collect(), total)
}
