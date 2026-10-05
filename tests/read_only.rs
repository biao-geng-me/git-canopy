//! canopy must never write to the repository: in particular it must not rewrite the index
//! (which takes index.lock and can make an agent's concurrent `git commit` fail).

mod common;

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use canopy::diff::{self, Target};
use canopy::watch::{self, Config};

fn index_state(wt: &Path) -> (SystemTime, Vec<u8>) {
    let admin = canopy::git::worktree::admin_dir(wt).unwrap();
    let idx = admin.join("index");
    (
        std::fs::metadata(&idx).unwrap().modified().unwrap(),
        std::fs::read(&idx).unwrap(),
    )
}

#[test]
fn refresh_and_diff_leave_index_untouched() {
    let fx = common::Fixture::new();
    let wts = [fx.repo(), fx.wt("feat-auth"), fx.wt("refactor-db")];
    // Make tracked files stat-dirty (same content, new mtime): porcelain `git diff` would
    // refresh and rewrite the index here.
    std::thread::sleep(Duration::from_millis(1100));
    for wt in &wts {
        let f = wt.join("README.md");
        let content = std::fs::read(&f).unwrap();
        std::fs::write(&f, content).unwrap();
    }
    let before: Vec<_> = wts.iter().map(|w| index_state(w)).collect();

    let paths = canopy::git::discover(&fx.repo()).unwrap();
    let cfg = Config {
        interval: Duration::from_millis(200),
        ..Config::default()
    };
    let (tx, rx) = mpsc::channel();
    let h = watch::spawn(paths, cfg, move |s| {
        let _ = tx.send(s);
    });
    // Let it run a few poll cycles.
    let deadline = std::time::Instant::now() + Duration::from_millis(1200);
    while std::time::Instant::now() < deadline {
        let _ = rx.recv_timeout(Duration::from_millis(100));
    }
    drop(h);
    for wt in &wts {
        let target = Target::Worktree {
            path: wt.clone(),
            has_head: true,
        };
        diff::load(&target).unwrap();
        diff::patch(&target, None).unwrap(); // `yd`
    }

    let after: Vec<_> = wts.iter().map(|w| index_state(w)).collect();
    for ((wt, b), a) in wts.iter().zip(&before).zip(&after) {
        assert!(b == a, "index of {} was rewritten", wt.display());
    }
}
