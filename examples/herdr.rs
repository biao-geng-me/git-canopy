//! Print what canopy sees from herdr for a repository, as it changes (read-only calls only:
//! `agent.list`, `workspace.list`, `events.subscribe`).
//!
//!     cargo run --example herdr -- [repo path] [seconds]

use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let common = canopy::git::discover(path.as_ref())
        .expect("not a git repository")
        .common_dir;
    let (wts, _) = canopy::git::worktree::list(&common).expect("worktree list");
    let paths: Vec<_> = wts.iter().map(|w| canopy::git::canonical(&w.path)).collect();
    println!("socket: {:?}", canopy::herdr::socket_path());

    let (tx, rx) = mpsc::channel();
    canopy::herdr::spawn(move |v| {
        let _ = tx.send(v);
    });
    let t0 = Instant::now();
    while let Ok(v) = rx.recv_timeout(Duration::from_secs(secs).saturating_sub(t0.elapsed())) {
        let ms = t0.elapsed().as_millis();
        let Some(v) = v else {
            println!("{ms:>6} ms  herdr unreachable");
            continue;
        };
        println!(
            "{ms:>6} ms  {} agents, {} workspaces",
            v.agents.len(),
            v.workspaces.len()
        );
        for (wi, agents) in canopy::herdr::assign(&v, &paths).iter().enumerate() {
            if let Some(a) = canopy::herdr::activity(&v, agents) {
                let names: Vec<_> = agents.iter().map(|&i| v.agents[i].label.as_str()).collect();
                println!(
                    "          {}  {:?} ×{}  {:?}",
                    paths[wi].display(),
                    a.state,
                    a.count,
                    names
                );
            }
        }
    }
}
