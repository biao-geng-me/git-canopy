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

    // The overlaps view: per shared file, this side's work (untracked here), then the other's.
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
    assert_eq!(doc.files.len(), 1);
    let f = &doc.files[0];
    assert_eq!((f.path.as_str(), f.status, f.adds), ("src/auth.rs", 'A', 1));
    assert_eq!(f.note.as_deref(), Some("↔ feat-auth"));
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
}
