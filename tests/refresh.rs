mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use canopy::model::Snapshot;
use canopy::watch::{self, Config};

fn wait_for(
    rx: &mpsc::Receiver<std::sync::Arc<Snapshot>>,
    timeout: Duration,
    pred: impl Fn(&Snapshot) -> bool,
) -> std::sync::Arc<Snapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let s = rx.recv_timeout(left).expect("timed out waiting for snapshot");
        if pred(&s) {
            return s;
        }
    }
}

fn run(watch_fs: bool) {
    let fx = common::Fixture::new();
    let paths = canopy::git::discover(&fx.repo()).unwrap();
    let cfg = Config {
        interval: Duration::from_millis(300),
        watch: watch_fs,
        ..Config::default()
    };
    let (tx, rx) = mpsc::channel();
    let _h = watch::spawn(paths, cfg, move |s| {
        let _ = tx.send(s);
    });
    let s = wait_for(&rx, Duration::from_secs(10), |s| s.statuses_loaded);
    let by = |s: &Snapshot, n: &str| s.worktrees.iter().find(|w| w.name == n).cloned().unwrap();
    assert_eq!(s.worktrees.len(), 8);
    assert_eq!(s.base.as_deref(), Some("main"));
    assert!(by(&s, "feat-auth").is_dirty());
    assert_eq!(by(&s, "feat-auth").vs_base, Some((2, 0)));
    assert_eq!(by(&s, "fix-parser").vs_base, Some((1, 1)));
    assert_eq!(by(&s, "conflict").op, Some("MERGING"));
    assert!(by(&s, "main").is_current);
    assert!(by(&s, "gone").status.is_none());

    // A working-tree edit is picked up by polling.
    std::fs::write(fx.wt("refactor-db").join("new.txt"), "x").unwrap();
    wait_for(&rx, Duration::from_secs(5), |s| by(s, "refactor-db").is_dirty());

    // A new commit moves ahead/behind.
    let ok = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qam", "more"])
        .current_dir(fx.wt("fix-parser"))
        .status()
        .unwrap();
    assert!(ok.success());
    wait_for(&rx, Duration::from_secs(5), |s| {
        let w = by(s, "fix-parser");
        w.vs_base == Some((2, 1)) && !w.is_dirty()
    });
}

#[test]
fn live_refresh_with_fs_watch() {
    run(true);
}

#[test]
fn live_refresh_polling_only() {
    run(false);
}

#[test]
fn graph_loads_more_history_on_request() {
    let fx = common::Fixture::new();
    let paths = canopy::git::discover(&fx.repo()).unwrap();
    let cfg = Config {
        interval: Duration::from_secs(60),
        page: 5,
        ..Config::default()
    };
    let (tx, rx) = mpsc::channel();
    let h = watch::spawn(paths, cfg, move |s| {
        let _ = tx.send(s);
    });
    let s = wait_for(&rx, Duration::from_secs(10), |s| !s.graph.commits.is_empty());
    assert_eq!(s.graph.commits.len(), 5);
    assert!(s.graph.truncated);
    h.request(watch::Request::LoadMore);
    let s = wait_for(&rx, Duration::from_secs(10), |s| s.graph.commits.len() == 10);
    assert!(s.graph.truncated);
    for _ in 0..3 {
        h.request(watch::Request::LoadMore);
    }
    let s = wait_for(&rx, Duration::from_secs(10), |s| !s.graph.truncated);
    assert!(s.graph.commits.len() < 25);
    // Every dirty worktree whose HEAD is loaded has an uncommitted row.
    let dirty = s.worktrees.iter().filter(|w| w.is_dirty()).count();
    let pseudo = s
        .graph
        .rows
        .iter()
        .filter(|r| matches!(r.kind, canopy::graph::RowKind::Uncommitted(_)))
        .count();
    assert_eq!(pseudo, dirty);
}

fn spawn(fx: &common::Fixture, cfg: Config) -> (watch::Handle, mpsc::Receiver<std::sync::Arc<Snapshot>>) {
    let paths = canopy::git::discover(&fx.repo()).unwrap();
    let (tx, rx) = mpsc::channel();
    let h = watch::spawn(paths, cfg, move |s| {
        let _ = tx.send(s);
    });
    (h, rx)
}

fn find(s: &Snapshot, name: &str) -> canopy::model::WorktreeView {
    s.worktrees.iter().find(|w| w.name == name).cloned().unwrap()
}

#[test]
fn worktree_edits_are_seen_without_polling() {
    let fx = common::Fixture::new();
    // Polling effectively off: only the worktree watch can notice the edit.
    let cfg = Config {
        interval: Duration::from_secs(3600),
        ..Config::default()
    };
    let (_h, rx) = spawn(&fx, cfg);
    let s = wait_for(&rx, Duration::from_secs(10), |s| s.worktrees.iter().any(|w| w.watched));
    let watched: Vec<_> = s
        .worktrees
        .iter()
        .filter(|w| w.has_worktree())
        .map(|w| w.watched)
        .collect();
    assert!(
        watched.iter().all(|&w| w),
        "every live worktree is watched: {watched:?}"
    );

    std::fs::write(fx.wt("refactor-db").join("src/db.rs"), "changed\n").unwrap();
    wait_for(&rx, Duration::from_secs(5), |s| find(s, "refactor-db").is_dirty());

    // A new directory gets watched too: a file created inside it is noticed.
    std::fs::create_dir_all(fx.wt("detached").join("new/deeper")).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(fx.wt("detached").join("new/deeper/file.txt"), "x").unwrap();
    wait_for(&rx, Duration::from_secs(5), |s| find(s, "detached").is_dirty());
}

#[test]
fn no_poll_updates_only_on_events_or_refresh() {
    let fx = common::Fixture::new();
    let cfg = Config {
        interval: Duration::from_millis(200),
        poll: false,
        watch: false,
        ..Config::default()
    };
    let (h, rx) = spawn(&fx, cfg);
    let s = wait_for(&rx, Duration::from_secs(10), |s| {
        s.statuses_loaded && !s.graph.rows.is_empty()
    });
    assert!(!s.polling);
    while rx.recv_timeout(Duration::from_millis(300)).is_ok() {}

    std::fs::write(fx.wt("refactor-db").join("new.txt"), "x").unwrap();
    let quiet = rx.recv_timeout(Duration::from_millis(1500));
    assert!(quiet.is_err(), "no polling: nothing may refresh by itself");

    h.request(watch::Request::Refresh);
    wait_for(&rx, Duration::from_secs(5), |s| find(s, "refactor-db").is_dirty());
}

#[test]
fn change_sizes_and_overlaps() {
    let fx = common::Fixture::new();
    let cfg = Config {
        interval: Duration::from_millis(300),
        ..Config::default()
    };
    let (_h, rx) = spawn(&fx, cfg);
    let s = wait_for(&rx, Duration::from_secs(10), |s| {
        s.worktrees.iter().filter(|w| w.changes.is_some()).count() >= 6
    });
    let size = |name: &str| {
        let ch = find(&s, name).changes.unwrap();
        (ch.adds, ch.dels)
    };
    // Committed and uncommitted together, vs the merge base with main.
    assert_eq!(size("feat-auth"), (7, 0));
    let files: Vec<String> = find(&s, "feat-auth")
        .changes
        .unwrap()
        .files
        .iter()
        .map(|f| f.path.clone())
        .collect();
    assert_eq!(
        files,
        [
            "src/auth.rs",
            "src/lib.rs",
            "src/main.rs",
            "src/session.rs",
            "src/token.rs"
        ]
    );
    assert_eq!(size("fix-parser"), (2, 0));
    assert_eq!(size("refactor-db"), (4, 0));
    assert_eq!(size("main"), (0, 0));

    // conflict and main both changed README.md since conflict branched off.
    let ov = find(&s, "conflict").overlaps;
    assert_eq!(ov.len(), 1, "{ov:?}");
    assert_eq!(
        (ov[0].with.as_str(), &ov[0].files[..]),
        ("main", &["README.md".to_string()][..])
    );
    assert!(find(&s, "main").overlaps.iter().any(|o| o.with == "conflict"));
    assert!(find(&s, "feat-auth").overlaps.is_empty());

    // Another worktree starts on a file feat-auth changed: both see the overlap.
    std::fs::write(fx.wt("refactor-db").join("src/auth.rs"), "pub fn login2() {}\n").unwrap();
    let s = wait_for(&rx, Duration::from_secs(5), |s| {
        find(s, "feat-auth").overlaps.iter().any(|o| o.with == "refactor-db")
    });
    let ov = find(&s, "refactor-db").overlaps;
    assert_eq!(ov[0].with, "feat-auth");
    assert_eq!(ov[0].files, ["src/auth.rs"]);

    // The overlaps view: one row per shared file, a column per side (this worktree first); the
    // entry shows this side's work (untracked here), then the other's.
    let target = canopy::diff::Target::Overlaps {
        path: fx.wt("refactor-db"),
        name: "refactor-db".into(),
        overlaps: ov,
    };
    let doc = canopy::diff::load(&target).unwrap();
    assert!(
        doc.overview.iter().any(|l| l == "feat-auth (1 file):"),
        "{:?}",
        doc.overview
    );
    assert_eq!(doc.columns, ["refactor-db", "feat-auth"]);
    assert_eq!(doc.files.len(), 1);
    let f = &doc.files[0];
    assert_eq!((f.path.as_str(), f.status, f.adds), ("src/auth.rs", 'A', 1));
    assert_eq!(f.cells, [Some((1, 0)), Some((3, 0))]);
    let heading = |prefix: &str| f.lines.iter().position(|l| l.starts_with(prefix)).unwrap();
    let (mine, theirs) = (
        heading("── refactor-db (this worktree), since "),
        heading("── feat-auth, since "),
    );
    assert!(mine < theirs);
    assert!(f.lines[mine..theirs].iter().any(|l| l == "+pub fn login2() {}"));
    assert!(
        f.lines[theirs..]
            .iter()
            .any(|l| l.starts_with('+') && l.contains("login"))
    );
    assert!(!f.lines[theirs..].iter().any(|l| l == "+pub fn login2() {}"));

    let patch = canopy::diff::patch(&target, Some("src/auth.rs")).unwrap();
    assert!(patch.starts_with("# refactor-db (this worktree), since "));
    assert!(patch.contains("\n# feat-auth, since ") && patch.contains("+pub fn login2() {}"));

    // A third worktree on the same file: still one row, now with a column per side.
    std::fs::write(fx.wt("fix-parser").join("src/auth.rs"), "a\nb\n").unwrap();
    let s = wait_for(&rx, Duration::from_secs(5), |s| {
        find(s, "feat-auth").overlaps.len() == 2
    });
    let w = find(&s, "feat-auth");
    let doc = canopy::diff::load(&canopy::diff::Target::Overlaps {
        path: w.path.clone(),
        name: w.name.clone(),
        overlaps: w.overlaps.clone(),
    })
    .unwrap();
    assert_eq!(doc.columns, ["feat-auth", "fix-parser", "refactor-db"]);
    let rows: Vec<_> = doc.files.iter().map(|f| (f.path.as_str(), f.cells.clone())).collect();
    assert_eq!(rows, [("src/auth.rs", vec![Some((3, 0)), Some((2, 0)), Some((1, 0))])]);
    assert_eq!(doc.files[0].lines.iter().filter(|l| l.starts_with("── ")).count(), 3);
}

/// Run git in `dir` with the user's config ignored and a fixed identity.
fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn merged_worktrees_are_marked_for_cleanup() {
    use canopy::model::{Cleanup, Landed};
    let fx = common::Fixture::new();
    let repo = fx.repo();
    let wt = |name: &str| fx.wt(name).to_string_lossy().into_owned();
    // ff-done: fast-forwarded into main. squashed: squash-merged into main.
    git(&repo, &["worktree", "add", "-q", "-b", "ff-done", &wt("ff-done"), "main"]);
    std::fs::write(fx.wt("ff-done").join("ff.txt"), "ff\n").unwrap();
    git(&fx.wt("ff-done"), &["add", "ff.txt"]);
    git(&fx.wt("ff-done"), &["commit", "-qm", "ff work"]);
    git(&repo, &["merge", "-q", "--ff-only", "ff-done"]);
    git(&repo, &["worktree", "add", "-q", "-b", "squashed", &wt("squashed"), "main"]);
    for line in ["one\n", "one\ntwo\n"] {
        std::fs::write(fx.wt("squashed").join("sq.txt"), line).unwrap();
        git(&fx.wt("squashed"), &["add", "sq.txt"]);
        git(&fx.wt("squashed"), &["commit", "-qm", "sq work"]);
    }
    git(&repo, &["merge", "-q", "--squash", "squashed"]);
    git(&repo, &["commit", "-qm", "squash of squashed"]);
    // Worktrees without commits of their own that git used just now are held back.
    fx.age(&fx.wt("detached"));
    fx.age(&fx.wt("untracked-only"));

    let cfg = Config {
        interval: Duration::from_millis(300),
        ..Config::default()
    };
    let (_h, rx) = spawn(&fx, cfg);
    let s = wait_for(&rx, Duration::from_secs(10), |s| {
        find(s, "ff-done").landed.is_some() && find(s, "squashed").landed.is_some()
    });
    let landed = |name: &str| find(&s, name).landed;
    let cleanup = |name: &str| find(&s, name).cleanup(s.base.as_deref(), false);
    assert_eq!(landed("ff-done"), Some(Landed::Merged));
    assert_eq!(landed("squashed"), Some(Landed::Applied));
    // docs sits at main with no commits of its own (and has untracked files); detached is a
    // checkout of an older main commit.
    assert_eq!(landed("docs"), Some(Landed::Unused));
    assert_eq!(landed("detached"), Some(Landed::Unused));
    for name in ["main", "feat-auth", "fix-parser", "refactor-db"] {
        assert_eq!(landed(name), None, "{name}");
    }
    assert_eq!(cleanup("ff-done"), Cleanup::Safe);
    assert_eq!(cleanup("squashed"), Cleanup::Safe);
    assert_eq!(find(&s, "squashed").cleanup(s.base.as_deref(), true), Cleanup::No);
    // main has squashed's work now, which is no overlap.
    assert!(find(&s, "squashed").overlaps.is_empty());
    assert!(!find(&s, "main").overlaps.iter().any(|o| o.with == "squashed"));
    // No commits of their own: nothing committed to lose, but docs' untracked files would be.
    assert_eq!(cleanup("detached"), Cleanup::Safe);
    assert_eq!(cleanup("docs"), Cleanup::Untracked);
    for name in ["main", "feat-auth", "refactor-db"] {
        assert_eq!(cleanup(name), Cleanup::No, "{name}");
    }

    // An untracked file would be lost; a modified one isn't merged work at all.
    std::fs::write(fx.wt("ff-done").join("scratch.txt"), "x").unwrap();
    std::fs::write(fx.wt("squashed").join("sq.txt"), "changed\n").unwrap();
    let s = wait_for(&rx, Duration::from_secs(5), |s| {
        find(s, "ff-done").is_dirty() && find(s, "squashed").is_dirty()
    });
    assert_eq!(find(&s, "ff-done").cleanup(s.base.as_deref(), false), Cleanup::Untracked);
    assert_eq!(find(&s, "squashed").cleanup(s.base.as_deref(), false), Cleanup::No);

    // New work on top of the squash: no longer merged.
    git(&fx.wt("squashed"), &["commit", "-qam", "more work"]);
    wait_for(&rx, Duration::from_secs(5), |s| {
        let w = find(s, "squashed");
        !w.is_dirty() && w.landed.is_none()
    });
}

#[test]
fn moved_head_refreshes_status() {
    // A commit made from another machine on a network filesystem: the branch moves but this
    // machine sees no event for the worktree's index or files. Only the refs change is noticed
    // (here by its event, otherwise by the refs poll), and that alone must refresh the status.
    let fx = common::Fixture::new();
    let cfg = Config {
        interval: Duration::from_secs(3600),
        ..Config::default()
    };
    let (_h, rx) = spawn(&fx, cfg);
    let s = wait_for(&rx, Duration::from_secs(10), |s| {
        s.statuses_loaded && s.worktrees.iter().any(|w| w.watched)
    });
    assert!(!find(&s, "refactor-db").is_dirty());

    // Move the branch back one commit from the main checkout: its files now differ from HEAD.
    git(&fx.repo(), &["update-ref", "refs/heads/refactor-db", "refactor-db~1"]);
    wait_for(&rx, Duration::from_secs(5), |s| find(s, "refactor-db").is_dirty());
}

#[test]
fn repositories_share_one_watcher() {
    // Two repositories watched through one hub: an edit reaches the worker of its own
    // repository (polling is effectively off, so only an event can explain the update).
    let (a, b) = (common::Fixture::new(), common::Fixture::new());
    let hub = watch::Hub::new();
    let cfg = Config {
        interval: Duration::from_secs(3600),
        ..Config::default()
    };
    let start = |fx: &common::Fixture| {
        let (tx, rx) = mpsc::channel();
        let paths = canopy::git::discover(&fx.repo()).unwrap();
        let h = watch::spawn_with(paths, cfg.clone(), hub.clone(), move |s| {
            let _ = tx.send(s);
        });
        (h, rx)
    };
    let ((_ha, rxa), (_hb, rxb)) = (start(&a), start(&b));
    let ready = |s: &Snapshot| s.statuses_loaded && s.worktrees.iter().any(|w| w.watched);
    wait_for(&rxa, Duration::from_secs(10), ready);
    wait_for(&rxb, Duration::from_secs(10), ready);

    std::fs::write(b.wt("refactor-db").join("new.txt"), "x").unwrap();
    wait_for(&rxb, Duration::from_secs(5), |s| find(s, "refactor-db").is_dirty());
    // a saw nothing of it.
    assert!(rxa.try_iter().all(|s| !find(&s, "refactor-db").is_dirty()));
    std::fs::write(a.wt("refactor-db").join("new.txt"), "x").unwrap();
    wait_for(&rxa, Duration::from_secs(5), |s| find(s, "refactor-db").is_dirty());
}
