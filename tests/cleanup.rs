//! Removing merged worktrees: with plain git, and through a fake herdr server (which removes the
//! checkout like herdr does and closes the workspace); refusals when something changed after the
//! confirm popup opened, an agent is busy, or the workspace is canopy's own.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use canopy::cleanup;
use canopy::herdr::json::Json;
use canopy::model::{Landed, WorktreeView};

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

#[test]
fn removes_checkout_and_branch_with_git() {
    let fx = common::Fixture::new();
    let w = merged_worktree(&fx, "ff-done");
    std::fs::write(fx.repo().join(".git/info/exclude"), "*.log\n").unwrap();
    std::fs::write(w.path.join("build.log"), "x").unwrap();

    let plan = cleanup::plan(&common_dir(&fx), &w, "main", None, None).unwrap();
    assert_eq!(plan.landed, Landed::Merged);
    assert!(plan.workspace.is_none());
    assert_eq!(plan.ignored, ["build.log"]);

    let notice = cleanup::run(&plan, None).unwrap();
    assert!(!w.path.exists());
    assert!(!branch_exists(&fx.repo(), "ff-done"));
    let short = &plan.head[..7];
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

    let w = view(&path, "squashed");
    let plan = cleanup::plan(&common_dir(&fx), &w, "main", None, None).unwrap();
    assert_eq!(plan.landed, Landed::Applied);
    cleanup::run(&plan, None).unwrap();
    assert!(!path.exists());
    assert!(!branch_exists(&repo, "squashed"));
}

#[test]
fn refuses_when_something_changed_after_planning() {
    let fx = common::Fixture::new();
    let common = common_dir(&fx);
    let w = merged_worktree(&fx, "ff-done");

    // Untracked file appears while the popup is open: nothing is removed.
    let plan = cleanup::plan(&common, &w, "main", None, None).unwrap();
    std::fs::write(w.path.join("new.txt"), "x").unwrap();
    let err = cleanup::run(&plan, None).unwrap_err().to_string();
    assert!(err.contains("untracked"), "{err}");
    assert!(w.path.exists() && branch_exists(&fx.repo(), "ff-done"));
    std::fs::remove_file(w.path.join("new.txt")).unwrap();

    // A new commit: HEAD moved, and the new work isn't in main.
    std::fs::write(w.path.join("more.txt"), "x").unwrap();
    git(&w.path, &["add", "."]);
    git(&w.path, &["commit", "-qm", "more"]);
    let err = cleanup::run(&plan, None).unwrap_err().to_string();
    assert!(err.contains("HEAD moved"), "{err}");
    let err = cleanup::plan(&common, &view(&w.path, "ff-done"), "main", None, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("isn't in main"), "{err}");
    assert!(w.path.exists() && branch_exists(&fx.repo(), "ff-done"));

}

#[test]
fn worktrees_without_commits_of_their_own_are_removed() {
    let fx = common::Fixture::new();
    let common = common_dir(&fx);

    // A fresh branch at main: nothing committed to lose; its branch goes too.
    let fresh = fx.wt("fresh");
    git(&fx.repo(), &["worktree", "add", "-q", "-b", "fresh", &fresh.to_string_lossy(), "main"]);
    let plan = cleanup::plan(&common, &view(&fresh, "fresh"), "main", None, None).unwrap();
    assert_eq!(plan.landed, Landed::Unused);
    cleanup::run(&plan, None).unwrap();
    assert!(!fresh.exists() && !branch_exists(&fx.repo(), "fresh"));

    // A detached checkout of an older main commit (the fixture's `detached`).
    let det = fx.wt("detached");
    let w = WorktreeView {
        path: canopy::git::canonical(&det),
        name: "detached".into(),
        head: Some(git(&det, &["rev-parse", "HEAD"])),
        ..Default::default()
    };
    let plan = cleanup::plan(&common, &w, "main", None, None).unwrap();
    assert_eq!((plan.landed, plan.branch.as_deref()), (Landed::Unused, None));
    cleanup::run(&plan, None).unwrap();
    assert!(!det.exists());

    // A new commit on a fresh branch is work of its own: refused.
    let wip = fx.wt("wip");
    git(&fx.repo(), &["worktree", "add", "-q", "-b", "wip", &wip.to_string_lossy(), "main"]);
    std::fs::write(wip.join("wip.txt"), "x").unwrap();
    git(&wip, &["add", "."]);
    git(&wip, &["commit", "-qm", "wip"]);
    let err = cleanup::plan(&common, &view(&wip, "wip"), "main", None, None).unwrap_err().to_string();
    assert!(err.contains("isn't in main"), "{err}");
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

    let plan = cleanup::plan(&common_dir(&fx), &w, "main", Some(herdr.sock.clone()), Some("w1")).unwrap();
    let ws = plan.workspace.as_ref().unwrap();
    assert_eq!((ws.id.as_str(), ws.label.as_str(), ws.panes), ("w9", "ff-done", 2));
    // The done agent is in that workspace: closed with it, not left behind.
    assert!(plan.stray_agents.is_empty());

    let notice = cleanup::run(&plan, Some("w1")).unwrap();
    assert_eq!(*herdr.calls.lock().unwrap(), ["remove w9 force=Some(Bool(false))"]);
    assert!(!w.path.exists());
    assert!(!branch_exists(&fx.repo(), "ff-done"), "herdr leaves the branch; canopy deletes it");
    assert!(notice.contains("herdr workspace ff-done"), "{notice}");
}

#[test]
fn herdr_refusals() {
    let fx = common::Fixture::new();
    let common = common_dir(&fx);
    let w = merged_worktree(&fx, "ff-done");

    let busy = FakeHerdr::start(&w.path, &fx.repo(), "working");
    let err = cleanup::plan(&common, &w, "main", Some(busy.sock.clone()), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("agent dev is working"), "{err}");

    let idle = FakeHerdr::start(&w.path, &fx.repo(), "idle");
    let err = cleanup::plan(&common, &w, "main", Some(idle.sock.clone()), Some("w9"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("canopy runs in its herdr workspace"), "{err}");
    assert!(idle.calls.lock().unwrap().is_empty() && busy.calls.lock().unwrap().is_empty());
    assert!(w.path.exists() && branch_exists(&fx.repo(), "ff-done"));
}
