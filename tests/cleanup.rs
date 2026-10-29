//! Cleaning up: merged worktrees (into base or any other branch) with plain git and through a
//! fake herdr server (which removes the checkout like herdr does and closes the workspace),
//! remote branches, branch labels without a worktree; refusals when something changed after the
//! popup opened, deleting would leave commits only in what else goes, an agent is busy, the
//! workspace is canopy's own, a checkout herdr made has no workspace this herdr knows, or a
//! worktree without commits was used just now.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use canopy::cleanup::{self, Plan, Target, What};
use canopy::herdr::json::Json;
use canopy::model::WorktreeView;

/// Run git in `dir` with the user's config ignored and a fixed identity; returns stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn branch_exists(repo: &Path, b: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")])
        .current_dir(repo)
        .output()
        .unwrap()
        .status
        .success()
}

/// A worktree `name` on branch `name` with one commit, fast-forwarded into main.
fn merged_worktree(fx: &common::Fixture, name: &str) -> WorktreeView {
    let repo = fx.repo();
    let path = fx.wt(name);
    git(&repo, &["worktree", "add", "-q", "-b", name, &path.to_string_lossy(), "main"]);
    std::fs::write(path.join(format!("{name}.txt")), "work\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-qm", "work"]);
    git(&repo, &["merge", "-q", "--ff-only", name]);
    view(&path, name)
}

fn view(path: &Path, branch: &str) -> WorktreeView {
    WorktreeView {
        path: canopy::git::canonical(path),
        name: branch.to_string(),
        branch: Some(branch.to_string()),
        head: Some(git(path, &["rev-parse", "HEAD"])),
        ..Default::default()
    }
}

fn common_dir(fx: &common::Fixture) -> PathBuf {
    canopy::git::discover(&fx.repo()).unwrap().common_dir
}

fn plan_for(fx: &common::Fixture, w: &WorktreeView) -> Plan {
    cleanup::plan(&common_dir(fx), Target::Worktree(Box::new(w.clone())), Some("main"), None, None, None).unwrap()
}

/// The plan's items, as `label` (allowed) or `label: why` (not).
fn items(plan: &Plan) -> Vec<String> {
    plan.items
        .iter()
        .map(|it| match &it.blocker {
            None => it.what.label(),
            Some(b) => format!("{}: {b}", it.what.label()),
        })
        .collect()
}

/// The labels of the items `chosen`.
fn chosen(plan: &Plan, chosen: &[bool]) -> Vec<String> {
    plan.items.iter().zip(chosen).filter(|&(_, &c)| c).map(|(it, _)| it.what.label()).collect()
}

fn index(plan: &Plan, label: &str) -> usize {
    plan.items.iter().position(|it| it.what.label() == label).unwrap_or_else(|| panic!("no {label} in {:?}", items(plan)))
}

#[test]
fn removes_checkout_and_branch_with_git() {
    let fx = common::Fixture::new();
    let w = merged_worktree(&fx, "ff-done");
    std::fs::write(fx.repo().join(".git/info/exclude"), "*.log\n").unwrap();
    std::fs::write(w.path.join("build.log"), "x").unwrap();

    let plan = plan_for(&fx, &w);
    let checkout = format!("checkout {}", w.path.display());
    assert_eq!(items(&plan), [checkout.clone(), "branch ff-done".into()]);
    let What::Checkout { ignored, .. } = &plan.items[0].what else { panic!() };
    assert_eq!(ignored, &["build.log"]);
    assert_eq!(plan.items[1].keepers, ["refs/heads/main"]);
    let all = plan.default_choice();
    assert_eq!(chosen(&plan, &all), [checkout, "branch ff-done".into()]);

    let notice = cleanup::run(&plan, &all, None).unwrap();
    assert!(!w.path.exists());
    assert!(!branch_exists(&fx.repo(), "ff-done"));
    let short = &w.head.as_ref().unwrap()[..7];
    assert!(notice.contains(&format!("git branch ff-done {short}")), "{notice}");
    assert!(!git(&fx.repo(), &["worktree", "list"]).contains("ff-done"));
}

#[test]
fn squash_merged_branch_is_removed_too() {
    let fx = common::Fixture::new();
    let repo = fx.repo();
    let path = fx.wt("squashed");
    git(&repo, &["worktree", "add", "-q", "-b", "squashed", &path.to_string_lossy(), "main"]);
    for line in ["one\n", "one\ntwo\n"] {
        std::fs::write(path.join("sq.txt"), line).unwrap();
        git(&path, &["add", "sq.txt"]);
        git(&path, &["commit", "-qm", "sq"]);
    }
    git(&repo, &["merge", "-q", "--squash", "squashed"]);
    git(&repo, &["commit", "-qm", "squash"]);

    let plan = plan_for(&fx, &view(&path, "squashed"));
    let b = index(&plan, "branch squashed");
    assert_eq!(plan.items[b].keepers, ["refs/heads/main"], "its content is in main");
    cleanup::run(&plan, &plan.default_choice(), None).unwrap();
    assert!(!path.exists());
    assert!(!branch_exists(&repo, "squashed"));
}

#[test]
fn merged_into_another_branch_than_base() {
    // An agent's branch merged into an integration branch that isn't main (yet).
    let fx = common::Fixture::new();
    let repo = fx.repo();
    let path = fx.wt("part");
    git(&repo, &["worktree", "add", "-q", "-b", "part", &path.to_string_lossy(), "main"]);
    std::fs::write(path.join("part.txt"), "x\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-qm", "part"]);
    git(&repo, &["branch", "integration", "part"]);

    let plan = plan_for(&fx, &view(&path, "part"));
    let b = index(&plan, "branch part");
    assert_eq!(plan.items[b].keepers, ["refs/heads/integration"]);
    assert!(plan.items[b].blocker.is_none());
    cleanup::run(&plan, &plan.default_choice(), None).unwrap();
    assert!(!path.exists() && !branch_exists(&repo, "part") && branch_exists(&repo, "integration"));
}

#[test]
fn two_branches_at_one_commit_are_not_both_deleted() {
    let fx = common::Fixture::new();
    let repo = fx.repo();
    let (x, y) = (fx.wt("x"), fx.wt("y"));
    git(&repo, &["worktree", "add", "-q", "-b", "x", &x.to_string_lossy(), "main"]);
    std::fs::write(x.join("x.txt"), "x\n").unwrap();
    git(&x, &["add", "."]);
    git(&x, &["commit", "-qm", "x"]);
    git(&repo, &["worktree", "add", "-q", "-b", "y", &y.to_string_lossy(), "x"]);

    // Each is "merged" into the other: the first goes, then the second is the only copy.
    let px = plan_for(&fx, &view(&x, "x"));
    assert_eq!(px.items[index(&px, "branch x")].keepers, ["refs/heads/y"]);
    cleanup::run(&px, &px.default_choice(), None).unwrap();
    let py = plan_for(&fx, &view(&y, "y"));
    let b = index(&py, "branch y");
    assert!(py.items[b].blocker.as_deref().unwrap().contains("not merged"), "{:?}", items(&py));
    assert!(py.default_choice().iter().all(|&c| !c), "nothing by default when the work isn't merged");
    // The checkout alone can still go: its branch keeps the work.
    let mut pick = vec![false; py.items.len()];
    py.toggle(&mut pick, index(&py, &format!("checkout {}", y.display()))).unwrap();
    assert!(py.toggle(&mut pick, b).is_err());
    cleanup::run(&py, &pick, None).unwrap();
    assert!(!y.exists() && branch_exists(&repo, "y"));

    // The same for two branch labels without a worktree, chosen together.
    git(&repo, &["branch", "p", "y"]);
    let sha = git(&repo, &["rev-parse", "y"]);
    let target = Target::Refs {
        sha,
        refs: vec!["refs/heads/p".into(), "refs/heads/y".into()],
    };
    let plan = cleanup::plan(&common_dir(&fx), target, Some("main"), None, None, None).unwrap();
    assert_eq!(chosen(&plan, &plan.default_choice()), ["branch p"]);
    assert!(plan.check(&[true, true]).is_err());
    cleanup::run(&plan, &plan.default_choice(), None).unwrap();
    assert!(!branch_exists(&repo, "p") && branch_exists(&repo, "y"));
}

#[test]
fn refuses_when_something_changed_after_planning() {
    let fx = common::Fixture::new();
    let w = merged_worktree(&fx, "ff-done");

    // Untracked file appears while the popup is open: nothing is removed.
    let plan = plan_for(&fx, &w);
    let all = plan.default_choice();
    std::fs::write(w.path.join("new.txt"), "x").unwrap();
    let err = cleanup::run(&plan, &all, None).unwrap_err().to_string();
    assert!(err.contains("untracked"), "{err}");
    assert!(w.path.exists() && branch_exists(&fx.repo(), "ff-done"));
    std::fs::remove_file(w.path.join("new.txt")).unwrap();

    // A new commit: HEAD moved, and the new work isn't in main.
    std::fs::write(w.path.join("more.txt"), "x").unwrap();
    git(&w.path, &["add", "."]);
    git(&w.path, &["commit", "-qm", "more"]);
    let err = cleanup::run(&plan, &all, None).unwrap_err().to_string();
    assert!(err.contains("HEAD moved"), "{err}");
    let plan = plan_for(&fx, &view(&w.path, "ff-done"));
    assert!(items(&plan)[1].contains("not merged"), "{:?}", items(&plan));
    assert!(w.path.exists() && branch_exists(&fx.repo(), "ff-done"));
}

#[test]
fn worktrees_without_commits_of_their_own_are_removed() {
    let fx = common::Fixture::new();

    // A fresh branch at main: nothing committed to lose; its branch goes too.
    let fresh = fx.wt("fresh");
    git(&fx.repo(), &["worktree", "add", "-q", "-b", "fresh", &fresh.to_string_lossy(), "main"]);
    // Just made: an agent may be about to start there.
    let plan = plan_for(&fx, &view(&fresh, "fresh"));
    assert!(items(&plan)[0].contains("an agent may be starting there"), "{:?}", items(&plan));
    assert!(plan.default_choice().iter().all(|&c| !c));
    fx.age(&fresh);
    let plan = plan_for(&fx, &view(&fresh, "fresh"));
    cleanup::run(&plan, &plan.default_choice(), None).unwrap();
    assert!(!fresh.exists() && !branch_exists(&fx.repo(), "fresh"));

    // A detached checkout of an older main commit (the fixture's `detached`).
    let det = fx.wt("detached");
    fx.age(&det);
    let w = WorktreeView {
        path: canopy::git::canonical(&det),
        name: "detached".into(),
        head: Some(git(&det, &["rev-parse", "HEAD"])),
        ..Default::default()
    };
    let plan = plan_for(&fx, &w);
    assert_eq!(plan.items.len(), 1, "{:?}", items(&plan));
    cleanup::run(&plan, &plan.default_choice(), None).unwrap();
    assert!(!det.exists());
}

#[test]
fn remote_branch_goes_when_another_remote_branch_has_it() {
    let fx = common::Fixture::new();
    let repo = fx.repo();
    let path = fx.wt("feat");
    git(&repo, &["worktree", "add", "-q", "-b", "feat", &path.to_string_lossy(), "main"]);
    std::fs::write(path.join("feat.txt"), "x\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-qm", "feat"]);
    git(&path, &["push", "-q", "-u", "origin", "feat"]);
    fx.age(&path);

    // Pushed, merged nowhere: the remote branch is the only copy there, and the local branch's
    // only other copy is its own upstream, so nothing is chosen by default.
    let plan = plan_for(&fx, &view(&path, "feat"));
    let r = index(&plan, "remote branch origin/feat");
    assert!(plan.items[r].blocker.as_deref().unwrap().contains("only copy"), "{:?}", items(&plan));
    assert!(plan.default_choice().iter().all(|&c| !c));
    assert_eq!(plan.items[index(&plan, "branch feat")].keepers, ["refs/remotes/origin/feat"]);

    // Merged into another branch on the remote (here: pushed as `integration`): everything goes.
    git(&path, &["push", "-q", "origin", "feat:integration"]);
    git(&repo, &["fetch", "-q", "origin"]);
    let plan = plan_for(&fx, &view(&path, "feat"));
    let all = plan.default_choice();
    assert_eq!(
        chosen(&plan, &all),
        [format!("checkout {}", path.display()), "branch feat".into(), "remote branch origin/feat".into()]
    );
    let notice = cleanup::run(&plan, &all, None).unwrap();
    assert!(notice.contains("origin/feat"), "{notice}");
    let origin = fx.dir.join("origin.git");
    assert!(git(&origin, &["branch", "--list", "feat"]).is_empty(), "deleted on the remote");
    assert!(!git(&origin, &["branch", "--list", "integration"]).is_empty());
    assert!(!branch_exists(&repo, "feat") && !path.exists());
}

#[test]
fn remote_branch_that_moved_on_the_remote_is_kept() {
    let fx = common::Fixture::new();
    let repo = fx.repo();
    git(&repo, &["push", "-q", "origin", "main~1:refs/heads/old", "main~1:refs/heads/keep"]);
    git(&repo, &["fetch", "-q", "origin"]);
    let sha = git(&repo, &["rev-parse", "main~1"]);
    let target = Target::Refs {
        sha,
        refs: vec!["refs/remotes/origin/old".into()],
    };
    let plan = cleanup::plan(&common_dir(&fx), target, Some("main"), None, None, None).unwrap();
    assert_eq!(items(&plan), ["remote branch origin/old"]);
    // Someone else pushes to it after the plan (this clone's origin/old doesn't know): the
    // lease keeps it.
    let main = git(&repo, &["rev-parse", "main"]);
    git(&fx.dir.join("origin.git"), &["update-ref", "refs/heads/old", &main]);
    let notice = cleanup::run(&plan, &plan.default_choice(), None).unwrap();
    assert!(notice.contains("kept origin/old"), "{notice}");
    assert!(!git(&fx.dir.join("origin.git"), &["branch", "--list", "old"]).is_empty());
}

#[test]
fn branch_labels_without_a_worktree() {
    let fx = common::Fixture::new();
    let repo = fx.repo();
    let common = common_dir(&fx);
    // `old-experiment` (fixture) isn't merged anywhere; `done` is in main.
    let sha = git(&repo, &["rev-parse", "old-experiment"]);
    let target = Target::Refs {
        sha,
        refs: vec!["refs/heads/old-experiment".into()],
    };
    let plan = cleanup::plan(&common, target, Some("main"), None, None, None).unwrap();
    assert!(items(&plan)[0].contains("not merged"), "{:?}", items(&plan));
    assert!(plan.default_choice().iter().all(|&c| !c));

    git(&repo, &["branch", "done", "main~1"]);
    let sha = git(&repo, &["rev-parse", "done"]);
    let target = Target::Refs {
        sha,
        refs: vec!["refs/heads/done".into(), "refs/heads/main".into()],
    };
    let plan = cleanup::plan(&common, target, Some("main"), None, None, None).unwrap();
    assert_eq!(items(&plan)[1], "branch main: it is the base branch");
    assert_eq!(chosen(&plan, &plan.default_choice()), ["branch done"]);
    cleanup::run(&plan, &plan.default_choice(), None).unwrap();
    assert!(!branch_exists(&repo, "done") && branch_exists(&repo, "main"));
}

/// A herdr stand-in: one workspace backed by `checkout`, one agent in it with `status`;
/// `worktree.remove` removes the checkout with git, as herdr does.
struct FakeHerdr {
    sock: PathBuf,
    calls: Arc<Mutex<Vec<String>>>,
}

impl FakeHerdr {
    fn start(checkout: &Path, repo: &Path, status: &'static str) -> FakeHerdr {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let sock = std::env::temp_dir().join(format!("canopy-cleanup-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (checkout, repo, log) = (checkout.to_string_lossy().into_owned(), repo.to_path_buf(), calls.clone());
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { break };
                let mut line = String::new();
                if BufReader::new(conn.try_clone().unwrap()).read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let req = Json::parse(line.trim()).unwrap();
                let id = req.get_str("id").unwrap().to_string();
                let method = req.get_str("method").unwrap().to_string();
                let reply = match method.as_str() {
                    "agent.list" => format!(
                        r#"{{"id":"{id}","result":{{"type":"agent_list","agents":[{{"agent":"claude","agent_status":"{status}","cwd":"{checkout}","foreground_cwd":"{checkout}","name":"dev","pane_id":"w9:p1","workspace_id":"w9"}}]}}}}"#
                    ),
                    "workspace.list" => format!(
                        r#"{{"id":"{id}","result":{{"type":"workspace_list","workspaces":[{{"workspace_id":"w9","label":"ff-done","pane_count":2,"worktree":{{"checkout_path":"{checkout}","is_linked_worktree":true}}}}]}}}}"#
                    ),
                    "worktree.remove" => {
                        let params = req.get("params").unwrap();
                        log.lock().unwrap().push(format!(
                            "remove {} force={:?}",
                            params.get_str("workspace_id").unwrap(),
                            params.get("force")
                        ));
                        git(&repo, &["worktree", "remove", &checkout]);
                        format!(r#"{{"id":"{id}","result":{{"type":"worktree_removed","workspace_id":"w9"}}}}"#)
                    }
                    "workspace.close" => {
                        let params = req.get("params").unwrap();
                        log.lock().unwrap().push(format!("close {}", params.get_str("workspace_id").unwrap()));
                        format!(r#"{{"id":"{id}","result":{{"type":"ok"}}}}"#)
                    }
                    other => panic!("unexpected call: {other}"),
                };
                let _ = writeln!(conn, "{reply}");
            }
        });
        FakeHerdr { sock, calls }
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock);
    }
}

#[test]
fn herdr_workspace_is_removed_through_herdr() {
    let fx = common::Fixture::new();
    let w = merged_worktree(&fx, "ff-done");
    let herdr = FakeHerdr::start(&w.path, &fx.repo(), "done");

    // herdr made the checkout (it is in herdr's worktree directory) and has its workspace.
    let herdr_dir = w.path.parent().unwrap();
    let target = Target::Worktree(Box::new(w.clone()));
    let plan = cleanup::plan(&common_dir(&fx), target, Some("main"), Some(herdr.sock.clone()), Some(herdr_dir), Some("w1")).unwrap();
    let What::Workspace(ws) = &plan.items[0].what else { panic!("{:?}", items(&plan)) };
    assert_eq!((ws.id.as_str(), ws.label.as_str(), ws.panes), ("w9", "ff-done", 2));
    // The done agent is in that workspace: closed with it, not left behind.
    let What::Checkout { stray_agents, .. } = &plan.items[1].what else { panic!() };
    assert!(stray_agents.is_empty());
    // A checkout goes with its workspace.
    assert_eq!(plan.items[1].requires, [0]);

    let notice = cleanup::run(&plan, &plan.default_choice(), Some("w1")).unwrap();
    assert_eq!(*herdr.calls.lock().unwrap(), ["remove w9 force=Some(Bool(false))"]);
    assert!(!w.path.exists());
    assert!(!branch_exists(&fx.repo(), "ff-done"), "herdr leaves the branch; canopy deletes it");
    assert!(notice.contains("herdr workspace ff-done"), "{notice}");
}

#[test]
fn herdr_workspace_alone_is_closed() {
    let fx = common::Fixture::new();
    let w = merged_worktree(&fx, "ff-done");
    let herdr = FakeHerdr::start(&w.path, &fx.repo(), "idle");
    let target = Target::Worktree(Box::new(w.clone()));
    let plan = cleanup::plan(&common_dir(&fx), target, Some("main"), Some(herdr.sock.clone()), None, None).unwrap();
    let mut pick = vec![false; plan.items.len()];
    plan.toggle(&mut pick, 0).unwrap();
    assert_eq!(chosen(&plan, &pick), ["herdr workspace ff-done"]);
    // Choosing the branch takes the checkout and the workspace with it; dropping the
    // workspace drops them again.
    let b = index(&plan, "branch ff-done");
    plan.toggle(&mut pick, b).unwrap();
    assert_eq!(pick, [true, true, true]);
    plan.toggle(&mut pick, 0).unwrap();
    assert_eq!(pick, [false, false, false]);
    plan.toggle(&mut pick, 0).unwrap();

    cleanup::run(&plan, &pick, None).unwrap();
    assert_eq!(*herdr.calls.lock().unwrap(), ["close w9"]);
    assert!(w.path.exists() && branch_exists(&fx.repo(), "ff-done"));
}

#[test]
fn herdr_refusals() {
    let fx = common::Fixture::new();
    let common = common_dir(&fx);
    let w = merged_worktree(&fx, "ff-done");
    let target = || Target::Worktree(Box::new(w.clone()));

    let busy = FakeHerdr::start(&w.path, &fx.repo(), "working");
    let plan = cleanup::plan(&common, target(), Some("main"), Some(busy.sock.clone()), None, None).unwrap();
    assert!(items(&plan)[0].contains("agent dev is working"), "{:?}", items(&plan));
    assert!(items(&plan)[1].contains("agent dev is working"), "{:?}", items(&plan));
    assert!(plan.default_choice().iter().all(|&c| !c));

    let idle = FakeHerdr::start(&w.path, &fx.repo(), "idle");
    let plan = cleanup::plan(&common, target(), Some("main"), Some(idle.sock.clone()), None, Some("w9")).unwrap();
    assert!(items(&plan)[0].contains("canopy runs in it"), "{:?}", items(&plan));
    assert!(plan.default_choice().iter().all(|&c| !c));

    // herdr made the checkout, but no workspace this herdr has is backed by it (or herdr isn't
    // reachable): another herdr, maybe on another machine, may have agents in it.
    let herdr_dir = w.path.parent().unwrap();
    let elsewhere = FakeHerdr::start(&fx.repo(), &fx.repo(), "idle");
    for sock in [Some(elsewhere.sock.clone()), None] {
        let plan = cleanup::plan(&common, target(), Some("main"), sock, Some(herdr_dir), None).unwrap();
        assert!(items(&plan)[0].contains("no herdr workspace here has it"), "{:?}", items(&plan));
    }
    assert!(idle.calls.lock().unwrap().is_empty() && busy.calls.lock().unwrap().is_empty());
    assert!(w.path.exists() && branch_exists(&fx.repo(), "ff-done"));
}
