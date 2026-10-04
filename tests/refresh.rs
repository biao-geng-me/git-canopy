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
        let s = rx
            .recv_timeout(left)
            .expect("timed out waiting for snapshot");
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
        base: None,
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
    wait_for(&rx, Duration::from_secs(5), |s| {
        by(s, "refactor-db").is_dirty()
    });

    // A new commit moves ahead/behind.
    let ok = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qam",
            "more",
        ])
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
