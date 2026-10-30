//! Cleaning up after work is done: a worktree's checkout, the herdr workspace backed by it, its
//! branch and that branch's upstream on the remote; or branches without a worktree (labels on a
//! graph commit). canopy's only writes, made only after the user picks them in a [`Plan`] (or
//! takes its default), and only after everything is checked again right before acting ([`run`]).
//!
//! Nothing is lost: every item that drops a commit ([`Item::sha`]) is offered only while another
//! ref that stays keeps it ([`Item::keepers`]): a branch merged into any other branch, local or
//! remote, not just the base branch. Choosing several items at once counts what they delete
//! together ([`Plan::check`]), so two branches at the same commit are never both deleted.
//! Nothing is forced either: `git worktree remove` (or herdr's `worktree.remove`, which runs it)
//! refuses a dirty checkout by itself, a branch is deleted only while it still points at the
//! checked commit, and a remote branch only with `--force-with-lease` at the checked commit.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::git::refs::{self, Upstream};
use crate::git::{self, changes};
use crate::herdr::{self, AgentState};
use crate::model::{Landed, WorktreeView};

/// Ignored entries listed in a plan (the rest is counted).
const MAX_IGNORED: usize = 200;

/// How long `ls-remote` and `push` get before they are given up.
const NET_TIMEOUT: Duration = Duration::from_secs(30);

/// What Backspace was pressed on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Target {
    Worktree(Box<WorktreeView>),
    /// A graph commit without a worktree: its branch labels, as full ref names
    /// (`refs/heads/x`, `refs/remotes/origin/x`).
    Refs { sha: String, refs: Vec<String> },
}

/// What cleaning up a target could delete, item by item, for the popup and for [`run`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub common_dir: PathBuf,
    pub target: Target,
    /// The worktree's name, or the first branch's.
    pub name: String,
    pub base: Option<String>,
    /// herdr socket, when herdr is in use and reachable.
    pub herdr: Option<PathBuf>,
    /// Where herdr creates worktrees (`herdr::worktree_dir`).
    pub herdr_dir: Option<PathBuf>,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub what: What,
    /// The commit that deleting this item drops a ref to (`None`: nothing committed at stake).
    pub sha: Option<String>,
    /// Full names of the refs that keep `sha`'s work when this item goes: refs containing it,
    /// or the base branch (or its upstream) when its content is there (squash-merged). At least
    /// one must stay for the item to be allowed.
    pub keepers: Vec<String>,
    /// Why this item can't be deleted at all (shown next to it).
    pub blocker: Option<String>,
    /// Why this item may be deleted only when picked by hand: never in [`Plan::default_choice`]
    /// (shown next to it).
    pub warning: Option<String>,
    /// Items that go with this one (indexes into [`Plan::items`]): a branch takes its checkout,
    /// a checkout the herdr workspace backed by it.
    pub requires: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum What {
    /// The herdr workspace backed by the checkout; closed alone with `workspace.close`, or
    /// together with the checkout by herdr's `worktree.remove`.
    Workspace(herdr::Workspace),
    Checkout {
        path: PathBuf,
        /// Ignored files and directories that go with it (first [`MAX_IGNORED`]).
        ignored: Vec<String>,
        ignored_total: usize,
        /// Idle or done agents working in it outside its workspace: their panes stay open, in
        /// a directory that no longer exists.
        stray_agents: Vec<String>,
    },
    /// A local branch (short name).
    Branch(String),
    /// A branch on a remote, deleted there with `git push`.
    Remote(Upstream),
}

impl What {
    /// The ref deleting this removes (none for a checkout or workspace).
    pub fn refname(&self) -> Option<String> {
        match self {
            What::Branch(b) => Some(format!("refs/heads/{b}")),
            What::Remote(u) => Some(u.tracking.clone()),
            What::Workspace(_) | What::Checkout { .. } => None,
        }
    }

    /// Same item in a fresher plan.
    fn same(&self, other: &What) -> bool {
        match (self, other) {
            (What::Workspace(a), What::Workspace(b)) => a.id == b.id,
            (What::Checkout { path: a, .. }, What::Checkout { path: b, .. }) => a == b,
            (What::Branch(a), What::Branch(b)) => a == b,
            (What::Remote(a), What::Remote(b)) => a == b,
            _ => false,
        }
    }

    /// `checkout`, `branch`, …
    pub fn kind(&self) -> &'static str {
        match self {
            What::Workspace(_) => "herdr workspace",
            What::Checkout { .. } => "checkout",
            What::Branch(_) => "branch",
            What::Remote(_) => "remote branch",
        }
    }

    /// For notices and the popup: `checkout ~/x`, `branch x`, `origin/x`.
    pub fn label(&self) -> String {
        match self {
            What::Workspace(ws) => format!("herdr workspace {}", ws.label),
            What::Checkout { path, .. } => format!("checkout {}", path.display()),
            What::Branch(b) => format!("branch {b}"),
            What::Remote(u) => format!("remote branch {}", u.short()),
        }
    }
}

impl Plan {
    /// Why item `i` can't be chosen at all: its own blocker, or that of an item it goes with.
    pub fn blocked(&self, i: usize) -> Option<String> {
        let item = self.items.get(i)?;
        if let Some(b) = &item.blocker {
            return Some(b.clone());
        }
        let mut todo = item.requires.clone();
        let mut seen = Vec::new();
        while let Some(j) = todo.pop() {
            if seen.contains(&j) {
                continue;
            }
            seen.push(j);
            let req = &self.items[j];
            if req.blocker.is_some() {
                return Some(format!("goes only with the {}, which can't go", req.what.kind()));
            }
            todo.extend(req.requires.iter().copied());
        }
        None
    }

    /// Whether deleting the items in `chosen` together loses nothing and each one may go.
    pub fn check(&self, chosen: &[bool]) -> Result<(), String> {
        let on = |i: usize| chosen.get(i).copied().unwrap_or(false);
        let gone: Vec<String> = (0..self.items.len())
            .filter(|&i| on(i))
            .filter_map(|i| self.items[i].what.refname())
            .collect();
        for item in self.items.iter().enumerate().filter(|&(i, _)| on(i)).map(|(_, it)| it) {
            if let Some(b) = &item.blocker {
                return Err(format!("{}: {b}", item.what.label()));
            }
            if let Some(&r) = item.requires.iter().find(|&&r| !on(r)) {
                return Err(format!("{} goes only with {}", item.what.label(), self.items[r].what.label()));
            }
            if item.sha.is_some() && !item.keepers.iter().any(|k| !gone.contains(k)) {
                let others: Vec<&str> = item.keepers.iter().map(|k| refs::short(k)).collect();
                return Err(format!(
                    "{}: its commits would be left only in what else is deleted ({})",
                    item.what.label(),
                    others.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Turn item `i` on (with the items it requires) or off (with the items that require it),
    /// unless the result fails [`Plan::check`] (then nothing changes and the reason is returned).
    pub fn toggle(&self, chosen: &mut [bool], i: usize) -> Result<(), String> {
        let mut next = chosen.to_vec();
        if next.get(i).copied().unwrap_or(false) {
            let mut off = vec![i];
            while let Some(j) = off.pop() {
                next[j] = false;
                off.extend((0..self.items.len()).filter(|&k| next[k] && self.items[k].requires.contains(&j)));
            }
        } else {
            let mut on = vec![i];
            while let Some(j) = on.pop() {
                if !next[j] {
                    next[j] = true;
                    on.extend(self.items[j].requires.iter().copied());
                }
            }
        }
        self.check(&next)?;
        chosen.copy_from_slice(&next);
        Ok(())
    }

    /// What a double Backspace deletes: every item that can go, in order, as long as they can
    /// all go together, except items with a warning (and those that go only with them). Nothing
    /// when the worktree's work isn't merged into another branch (only its own upstream has it,
    /// or nothing does): removing just the checkout, or the local branch of pushed work, is then
    /// something to pick in the popup.
    pub fn default_choice(&self) -> Vec<bool> {
        let mut chosen = vec![false; self.items.len()];
        let upstream: Vec<String> = self
            .items
            .iter()
            .filter_map(|it| match &it.what {
                What::Remote(u) => Some(u.tracking.clone()),
                _ => None,
            })
            .collect();
        for i in 0..self.items.len() {
            if !chosen[i] {
                let mut next = chosen.clone();
                let warned = |c: &[bool]| c.iter().zip(&self.items).any(|(&c, it)| c && it.warning.is_some());
                if self.toggle(&mut next, i).is_ok() && !warned(&next) {
                    chosen = next;
                }
            }
        }
        // A worktree's cleanup is all or nothing by default: its branch goes (with its checkout
        // and workspace), merged into another branch than its own upstream, or nothing does.
        if let Target::Worktree(w) = &self.target
            && w.branch.is_some()
        {
            let branch_goes = self.items.iter().zip(&chosen).any(|(it, &c)| {
                c && matches!(it.what, What::Branch(_)) && it.keepers.iter().any(|k| !upstream.contains(k))
            });
            if !branch_goes {
                chosen.iter_mut().for_each(|c| *c = false);
            }
        }
        chosen
    }
}

/// Look at `target` again and work out what cleaning it up could delete. `base`: the base
/// branch; `herdr`: the socket when herdr is in use; `herdr_dir`: where herdr creates worktrees;
/// `own_workspace`: the herdr workspace canopy itself runs in (never closed).
pub fn plan(
    common_dir: &Path,
    target: Target,
    base: Option<&str>,
    herdr: Option<PathBuf>,
    herdr_dir: Option<&Path>,
    own_workspace: Option<&str>,
) -> Result<Plan> {
    let view = herdr.as_deref().and_then(|p| herdr::read_view_at(p).ok());
    let herdr = herdr.filter(|_| view.is_some());
    let mut items = Vec::new();
    let name = match &target {
        Target::Worktree(w) => {
            worktree_items(common_dir, w, base, view.as_ref(), herdr_dir, own_workspace, &mut items)?;
            w.name.clone()
        }
        Target::Refs { refs: names, .. } => {
            ref_items(common_dir, names, base, &mut items)?;
            names.first().map_or_else(String::new, |r| refs::short(r).to_string())
        }
    };
    Ok(Plan {
        common_dir: common_dir.to_path_buf(),
        target,
        name,
        base: base.map(str::to_string),
        herdr,
        herdr_dir: herdr_dir.map(Path::to_path_buf),
        items,
    })
}

/// `refs` in the order they are worth naming: the base branch, the main worktree's branch,
/// other local branches, then remote ones (each alphabetically).
fn sort_keepers(dir: &Path, base: Option<&str>, refs: &mut [String]) {
    let main = git::worktree::list(dir)
        .ok()
        .and_then(|(entries, _)| entries.first().and_then(|e| e.branch_name().map(|b| format!("refs/heads/{b}"))));
    let base = base.map(|b| format!("refs/heads/{b}"));
    refs.sort_by_key(|r| {
        (
            Some(r) != base.as_ref(),
            Some(r) != main.as_ref(),
            !r.starts_with("refs/heads/"),
            r.clone(),
        )
    });
}

fn worktree_items(
    dir: &Path,
    w: &WorktreeView,
    base: Option<&str>,
    view: Option<&herdr::View>,
    herdr_dir: Option<&Path>,
    own_workspace: Option<&str>,
    items: &mut Vec<Item>,
) -> Result<()> {
    let head = w.head.clone().context("no HEAD")?;
    let mut containing = refs::containing(dir, &head)?;
    if let Some(b) = squashed_into(dir, base, &head, w.branch.as_deref()) {
        containing.push(b);
    }
    sort_keepers(dir, base, &mut containing);

    // The herdr workspace backed by the checkout, and the agents in it or in the checkout.
    let ws = view.and_then(|v| v.workspaces.iter().find(|ws| ws.checkout.as_deref() == Some(&w.path)).cloned());
    let mut busy = None;
    let mut stray = Vec::new();
    for a in view.map_or(&[][..], |v| &v.agents[..]) {
        let in_ws = ws.as_ref().is_some_and(|ws| ws.id == a.workspace_id);
        if !in_ws && !a.cwd.as_deref().is_some_and(|c| c.starts_with(&w.path)) {
            continue;
        }
        if !matches!(a.state, AgentState::Idle | AgentState::Done) {
            busy.get_or_insert_with(|| format!("agent {} is {} in it", a.label, a.state.word()));
        } else if !in_ws {
            stray.push(format!("{} ({}, pane {})", a.label, a.state.word(), a.pane_id));
        }
    }
    let ws_index = ws.as_ref().map(|_| items.len());
    if let Some(ws) = ws {
        let blocker = if own_workspace == Some(ws.id.as_str()) {
            Some("canopy runs in it".to_string())
        } else {
            busy.clone()
        };
        items.push(Item {
            what: What::Workspace(ws),
            sha: None,
            keepers: Vec::new(),
            blocker,
            warning: None,
            requires: Vec::new(),
        });
    }

    // The checkout: its committed work stays in its branch (or whatever contains a detached
    // HEAD); what can't be recovered is what isn't committed.
    let st = if w.has_worktree() { Some(git::status::load(&w.path)) } else { None };
    let mut blocker = checkout_blocker(w, st.as_ref(), &head);
    if blocker.is_none() {
        blocker = busy.clone();
    }
    let mut warning = None;
    if blocker.is_none() && ws_index.is_none() && herdr_dir.is_some_and(|d| w.path.starts_with(d)) {
        let used = git::worktree::admin_dir(&w.path).and_then(|a| git::worktree::last_used(&a));
        match crate::model::herdr_unseen(used.map(|t| crate::model::now_secs() - t)) {
            Ok(w) => warning = Some(w),
            Err(b) => blocker = Some(b),
        }
    }
    if blocker.is_none()
        && let Some(ago) = fresh_for(dir, w, &containing)
    {
        blocker = Some(format!("no commits yet and git used it {} min ago: an agent may be starting there", ago / 60));
    }
    let (ignored, ignored_total) = if blocker.is_none() { ignored(&w.path) } else { (Vec::new(), 0) };
    let checkout = items.len();
    items.push(Item {
        what: What::Checkout {
            path: w.path.clone(),
            ignored,
            ignored_total,
            stray_agents: stray,
        },
        sha: Some(head.clone()),
        keepers: containing.clone(),
        blocker,
        warning,
        requires: ws_index.into_iter().collect(),
    });

    if let Some(b) = &w.branch {
        branch_items(dir, b, &head, containing, base, Some(checkout), items)?;
    }
    Ok(())
}

/// Why the checkout itself can't be removed (status `st`, freshly loaded).
fn checkout_blocker(w: &WorktreeView, st: Option<&Result<git::status::Status>>, head: &str) -> Option<String> {
    let reason = if !w.has_worktree() {
        "no checkout to remove".to_string()
    } else if w.is_main {
        "the main worktree can't be removed".to_string()
    } else if w.is_current {
        "canopy runs in it".to_string()
    } else if w.locked.is_some() {
        "it is locked".to_string()
    } else if let Some(op) = w.admin_dir.as_deref().and_then(git::worktree::operation) {
        format!("{op} in progress")
    } else {
        match st? {
            Err(e) => format!("{e:#}"),
            Ok(st) if st.oid.as_deref() != Some(head) || st.branch != w.branch => "its HEAD moved".to_string(),
            Ok(st) if st.staged + st.modified + st.unmerged > 0 => "it has uncommitted changes".to_string(),
            Ok(st) if st.untracked > 0 => {
                format!("{} untracked file(s) would be lost: delete them first (s opens a shell there)", st.untracked)
            }
            Ok(_) => return None,
        }
    };
    Some(reason)
}

/// A local branch `b` at `sha` and its upstream. `containing`: refs whose history has `sha` (or
/// its content); `checkout`: the item of the worktree it is checked out in.
fn branch_items(
    dir: &Path,
    b: &str,
    sha: &str,
    containing: Vec<String>,
    base: Option<&str>,
    checkout: Option<usize>,
    items: &mut Vec<Item>,
) -> Result<()> {
    let own = format!("refs/heads/{b}");
    let keepers: Vec<String> = containing.into_iter().filter(|r| *r != own).collect();
    let blocker = if Some(b) == base {
        Some("it is the base branch".to_string())
    } else if keepers.is_empty() {
        Some("not merged: no other branch has its commits".to_string())
    } else {
        None
    };
    items.push(Item {
        what: What::Branch(b.to_string()),
        sha: Some(sha.to_string()),
        keepers,
        blocker,
        warning: None,
        requires: checkout.into_iter().collect(),
    });
    if let Some(up) = refs::upstream(dir, b)? {
        remote_item(dir, up, base, items)?;
    }
    Ok(())
}

/// A branch on a remote, by its remote-tracking ref. Its commits must be on the remote in
/// another branch (any remote), so deleting it never takes away the only copy there.
fn remote_item(dir: &Path, up: Upstream, base: Option<&str>, items: &mut Vec<Item>) -> Result<()> {
    if items.iter().any(|it| it.what == What::Remote(up.clone())) {
        return Ok(());
    }
    let base_up = base.and_then(|b| refs::upstream(dir, b).ok().flatten());
    let Some(sha) = refs::resolve(dir, &up.tracking) else {
        items.push(Item {
            what: What::Remote(up),
            sha: None,
            keepers: Vec::new(),
            blocker: Some("already gone here (deleted on the remote, or never fetched)".to_string()),
            warning: None,
            requires: Vec::new(),
        });
        return Ok(());
    };
    let mut keepers: Vec<String> = refs::containing(dir, &sha)?
        .into_iter()
        .filter(|r| r.starts_with("refs/remotes/") && *r != up.tracking)
        .collect();
    if let Some(bu) = &base_up {
        // The base branch's upstream first.
        keepers.sort_by_key(|r| (*r != bu.tracking, r.clone()));
    }
    if keepers.is_empty()
        && let Some(bu) = &base_up
        && let Some(bu_sha) = refs::resolve(dir, &bu.tracking)
        && matches!(crate::cleanup::landed(dir, &bu_sha, &sha, None), Ok(Some(Landed::Applied)))
    {
        keepers.push(bu.tracking.clone());
    }
    let blocker = if base_up.as_ref().is_some_and(|bu| bu.tracking == up.tracking) {
        Some("it is the base branch's upstream".to_string())
    } else if keepers.is_empty() {
        Some("no other branch on the remote has its commits: it is the only copy there".to_string())
    } else {
        None
    };
    items.push(Item {
        what: What::Remote(up),
        sha: Some(sha),
        keepers,
        blocker,
        warning: None,
        requires: Vec::new(),
    });
    Ok(())
}

/// Branch labels without a worktree: each local branch with its upstream, and each remote
/// branch.
fn ref_items(dir: &Path, names: &[String], base: Option<&str>, items: &mut Vec<Item>) -> Result<()> {
    let (_, raw) = git::worktree::list(dir)?;
    let checked_out: Vec<String> = git::worktree::parse(&raw)
        .iter()
        .filter_map(|e| e.branch_name().map(str::to_string))
        .collect();
    for name in names {
        let Some(sha) = refs::resolve(dir, name) else { continue };
        if let Some(b) = name.strip_prefix("refs/heads/") {
            let mut containing = refs::containing(dir, &sha)?;
            if let Some(k) = squashed_into(dir, base, &sha, Some(b)) {
                containing.push(k);
            }
            sort_keepers(dir, base, &mut containing);
            let first = items.len();
            branch_items(dir, b, &sha, containing, base, None, items)?;
            if checked_out.iter().any(|c| c == b) && items[first].blocker.is_none() {
                items[first].blocker = Some("it is checked out in a worktree".to_string());
            }
        } else if let Some(up) = refs::remote_of(dir, name)? {
            remote_item(dir, up, base, items)?;
        }
    }
    if items.is_empty() {
        bail!("no branch here to delete");
    }
    Ok(())
}

/// The base branch's ref when `sha`'s work isn't contained in any branch but its content is in
/// base (squash-merged, rebased, cherry-picked).
fn squashed_into(dir: &Path, base: Option<&str>, sha: &str, branch: Option<&str>) -> Option<String> {
    let base = base?;
    let base_ref = format!("refs/heads/{base}");
    let base_sha = refs::resolve(dir, &base_ref)?;
    matches!(landed(dir, &base_sha, sha, branch), Ok(Some(Landed::Applied))).then_some(base_ref)
}

/// How long ago git used a worktree with no commits of its own (its branch never moved, or it
/// is detached on a commit some branch has), while that is under `model::FRESH_SECS`.
fn fresh_for(dir: &Path, w: &WorktreeView, containing: &[String]) -> Option<i64> {
    let own = w.branch.as_ref().map(|b| format!("refs/heads/{b}"));
    if !containing.iter().any(|r| Some(r) != own.as_ref()) {
        return None;
    }
    let moved = w.branch.as_deref().is_some_and(|b| changes::reflog_len(dir, b, 2).is_ok_and(|n| n >= 2));
    if moved {
        return None;
    }
    let used = git::worktree::admin_dir(&w.path).and_then(|a| git::worktree::last_used(&a));
    let v = WorktreeView {
        landed: Some(Landed::Unused),
        git_used_at: used,
        ..Default::default()
    };
    v.fresh_for()
}

/// Carry out the items `chosen` in `plan` after checking them once more against a fresh plan.
/// Returns the notice for the footer.
pub fn run(plan: &Plan, chosen: &[bool], own_workspace: Option<&str>) -> Result<String> {
    if !chosen.iter().any(|&c| c) {
        bail!("nothing chosen");
    }
    let fresh = self::plan(
        &plan.common_dir,
        plan.target.clone(),
        plan.base.as_deref(),
        plan.herdr.clone(),
        plan.herdr_dir.as_deref(),
        own_workspace,
    )?;
    let mut now = vec![false; fresh.items.len()];
    for (item, _) in plan.items.iter().zip(chosen).filter(|&(_, &c)| c) {
        let Some(j) = fresh.items.iter().position(|f| f.what.same(&item.what)) else {
            bail!("{} is no longer there", item.what.label());
        };
        if fresh.items[j].sha != item.sha {
            bail!("{} moved", item.what.label());
        }
        now[j] = true;
    }
    if plan.herdr.is_some() && fresh.herdr.is_none() {
        bail!("herdr didn't answer");
    }
    fresh.check(&now).map_err(anyhow::Error::msg)?;

    let pick = |f: fn(&What) -> bool| fresh.items.iter().zip(&now).filter(move |&(it, &c)| c && f(&it.what)).map(|(it, _)| it);
    let ws = pick(|w| matches!(w, What::Workspace(_))).next();
    let checkout = pick(|w| matches!(w, What::Checkout { .. })).next();
    let mut done: Vec<String> = Vec::new();
    match (ws.map(|it| &it.what), checkout.map(|it| &it.what), &fresh.herdr) {
        (Some(What::Workspace(ws)), Some(_), Some(sock)) => {
            herdr::remove_worktree_at(sock, &ws.id).context("herdr")?;
            done.push(format!("herdr workspace {} and its checkout", ws.label));
        }
        (Some(What::Workspace(ws)), None, Some(sock)) => {
            herdr::close_workspace_at(sock, &ws.id).context("herdr")?;
            done.push(format!("herdr workspace {}", ws.label));
        }
        (None, Some(What::Checkout { path, .. }), _) => {
            git::run(&fresh.common_dir, &["worktree", "remove", &path.to_string_lossy()])?;
            done.push("checkout".to_string());
        }
        (None, None, _) => {}
        _ => bail!("herdr isn't reachable"),
    }
    let mut restore = Vec::new();
    for item in pick(|w| matches!(w, What::Branch(_))) {
        let (What::Branch(b), Some(sha)) = (&item.what, &item.sha) else { continue };
        // Only while it still points at the checked commit.
        if refs::resolve(&fresh.common_dir, &format!("refs/heads/{b}")).as_ref() != Some(sha) {
            done.push(format!("kept branch {b}: it moved"));
            continue;
        }
        git::run(&fresh.common_dir, &["branch", "-D", b])?;
        let short = &sha[..sha.len().min(7)];
        done.push(format!("branch {b}"));
        restore.push(format!("git branch {b} {short}"));
    }
    for item in pick(|w| matches!(w, What::Remote(_))) {
        let (What::Remote(up), Some(sha)) = (&item.what, &item.sha) else { continue };
        match delete_remote(&fresh.common_dir, up, sha, &item.keepers) {
            Ok(()) => done.push(up.short().to_string()),
            Err(e) => done.push(format!("kept {}: {e:#}", up.short())),
        }
    }
    let mut notice = format!("{}: removed {}", plan.name, done.join(", "));
    if !restore.is_empty() {
        notice.push_str(&format!(" · {} restores", restore.join("; ")));
    }
    Ok(notice)
}

/// Delete branch `up` on its remote, if it is still at `sha` there, once one of `keepers` is
/// confirmed on its remote at the commit it has here (so it really holds `sha`'s work).
fn delete_remote(dir: &Path, up: &Upstream, sha: &str, keepers: &[String]) -> Result<()> {
    let mut confirmed = false;
    for k in keepers {
        let (Some(k_up), Some(k_sha)) = (refs::remote_of(dir, k)?, refs::resolve(dir, k)) else { continue };
        let out = git::run_net(dir, &["ls-remote", &k_up.remote, &format!("refs/heads/{}", k_up.branch)], NET_TIMEOUT)?;
        if String::from_utf8_lossy(&out).split_whitespace().next() == Some(k_sha.as_str()) {
            confirmed = true;
            break;
        }
    }
    if !confirmed {
        bail!("no other remote branch has its commits there now (fetch, then try again)");
    }
    let lease = format!("--force-with-lease=refs/heads/{}:{sha}", up.branch);
    git::run_net(dir, &["push", "--porcelain", &lease, &up.remote, &format!(":refs/heads/{}", up.branch)], NET_TIMEOUT)?;
    Ok(())
}

/// Whether `head`'s work is in the base branch (at `base_sha`); see `model::Landed`.
pub fn landed(dir: &Path, base_sha: &str, head: &str, branch: Option<&str>) -> Result<Option<Landed>> {
    let (_, ahead) = git::left_right_count(dir, base_sha, head)?;
    if ahead == 0 {
        // Nothing committed to lose; the reflog only tells merged work from none.
        let moved = branch.is_some_and(|b| changes::reflog_len(dir, b, 2).is_ok_and(|n| n >= 2));
        return Ok(Some(if moved { Landed::Merged } else { Landed::Unused }));
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
