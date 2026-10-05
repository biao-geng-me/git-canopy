//! Measure how long the refresh worker takes to produce each startup snapshot.
//!
//!     cargo run --release --example startup -- <repo path>

use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let t0 = Instant::now();
    let paths = canopy::git::discover(path.as_ref()).expect("not a git repository");
    let (tx, rx) = mpsc::channel();
    let cfg = canopy::watch::Config {
        interval: Duration::from_secs(3600),
        ..Default::default()
    };
    let _h = canopy::watch::spawn(paths, cfg, move |s| {
        let _ = tx.send(s);
    });
    while let Ok(s) = rx.recv_timeout(Duration::from_secs(60)) {
        let dirty = s.worktrees.iter().filter(|w| w.is_dirty()).count();
        let changes = s.worktrees.iter().filter(|w| w.changes.is_some()).count();
        let overlaps: usize = s.worktrees.iter().map(|w| w.overlaps.len()).sum();
        println!(
            "{:>7.1} ms  worktrees={} dirty={} statuses_loaded={} commits={} rows={} changes={} overlaps={}",
            t0.elapsed().as_secs_f64() * 1000.0,
            s.worktrees.len(),
            dirty,
            s.statuses_loaded,
            s.graph.commits.len(),
            s.graph.rows.len(),
            changes,
            overlaps / 2
        );
        // The last startup snapshot: change sizes and overlaps.
        if changes > 0 {
            break;
        }
    }
}
