//! `canopy serve` and its client, run locally (the same as over ssh, minus ssh).

mod common;

use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use canopy::diff::Target;
use canopy::remote::client::{self, Event};
use canopy::watch::{Config, Request};

const BIN: &str = env!("CARGO_BIN_EXE_canopy");

fn serve(paths: &[std::path::PathBuf]) -> Command {
    let cfg = Config {
        interval: Duration::from_millis(300),
        ..Config::default()
    };
    let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let mut cmd = Command::new(BIN);
    cmd.args(client::serve_args(&cfg, false, &paths));
    cmd
}

fn wait<T>(rx: &mpsc::Receiver<Event>, what: &str, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        if let Event::Closed(why) = &ev {
            panic!("closed while waiting for {what}: {why}");
        }
        if let Some(t) = pick(ev) {
            return t;
        }
    }
}

fn closed(rx: &mpsc::Receiver<Event>) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("timed out waiting for the end") {
            Event::Closed(why) => return why,
            _ => continue,
        }
    }
}

fn answer<T: Send + 'static>(f: impl FnOnce(canopy::watch::Reply<T>)) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    f(Box::new(move |r| {
        let _ = tx.send(r);
    }));
    rx.recv_timeout(Duration::from_secs(10)).expect("no reply")
}

#[test]
fn snapshots_and_calls_over_the_wire() {
    let fx = common::Fixture::new();
    let (tx, rx) = mpsc::channel();
    let paths = [fx.repo(), fx.wt("feat-auth"), fx.dir.join("nope")];
    let conn = client::connect(serve(&paths), move |e| {
        let _ = tx.send(e);
    })
    .unwrap();

    wait(&rx, "hello", |e| matches!(e, Event::Hello { .. }).then_some(()));
    let repos = wait(&rx, "repos", |e| match e {
        Event::Repos(r) => Some(r),
        _ => None,
    });
    assert_eq!(repos.len(), 3);
    // Two paths in one repository are one repository.
    let id = repos[0].as_ref().unwrap().id;
    assert_eq!(repos[1].as_ref().unwrap().id, id);
    assert!(repos[2].as_ref().unwrap_err().contains("nope"));

    let snap = wait(&rx, "statuses and graph", |e| match e {
        Event::Snapshot(r, s) if r == id && s.statuses_loaded && !s.graph.rows.is_empty() => Some(s),
        _ => None,
    });
    let by = |s: &canopy::model::Snapshot, n: &str| s.worktrees.iter().find(|w| w.name == n).cloned().unwrap();
    assert_eq!(snap.worktrees.len(), 8);
    assert!(by(&snap, "feat-auth").is_dirty());
    assert_eq!(
        by(&snap, "conflict").op,
        Some(canopy::git::worktree::Operation::Merging)
    );
    assert_eq!(
        snap.graph.rows.len(),
        snap.graph.commits.len() + snap.worktrees.iter().filter(|w| w.is_dirty()).count()
    );

    // Calls answer like a local handle would.
    let h = conn.handle(id);
    let main = by(&snap, "main").head.unwrap();
    let doc = answer(|r| {
        h.doc(
            Target::Commit {
                dir: snap.common_dir.clone(),
                id: main.clone(),
            },
            r,
        )
    })
    .unwrap();
    assert!(!doc.overview.is_empty());
    let msg = answer(|r| h.message(main.clone(), r)).unwrap();
    assert!(!msg.is_empty());
    let target = Target::Worktree {
        path: fx.wt("feat-auth"),
        has_head: true,
    };
    let patch = answer(|r| h.patch(target, None, r)).unwrap();
    assert!(patch.contains("diff --git"));
    let err = answer(|r| h.message("0000000000000000000000000000000000000bad".into(), r)).unwrap_err();
    assert!(!err.is_empty());

    // Later snapshots that don't carry the graph keep the last one.
    std::fs::write(fx.wt("refactor-db").join("new.txt"), "x").unwrap();
    h.request(Request::Refresh);
    let later = wait(&rx, "an edit", |e| match e {
        Event::Snapshot(_, s) if by(&s, "refactor-db").is_dirty() => Some(s),
        _ => None,
    });
    assert!(!later.graph.rows.is_empty());

    // Dropping the connection stops the server.
    drop((conn, h));
    let why = closed(&rx);
    assert!(why.contains("closed"), "{why}");
}

#[test]
fn login_noise_before_the_hello_is_skipped() {
    let fx = common::Fixture::new();
    let inner = serve(&[fx.repo()]);
    let args: Vec<String> = std::iter::once(inner.get_program())
        .chain(inner.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        "echo 'Welcome to the cluster'; echo '{\"not\": \"hello\"}'; exec \"$@\"",
        "sh",
    ])
    .args(args);
    let (tx, rx) = mpsc::channel();
    let _conn = client::connect(cmd, move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let host = wait(&rx, "hello", |e| match e {
        Event::Hello { host, .. } => Some(host),
        _ => None,
    });
    assert!(!host.is_empty());
}

#[test]
fn other_protocol_or_no_server_is_reported() {
    let (tx, rx) = mpsc::channel();
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        r#"echo '{"Hello":{"protocol":999,"version":"9.9.9","host":"far"}}'; sleep 5"#,
    ]);
    let _conn = client::connect(cmd, move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let why = closed(&rx);
    assert!(why.contains("protocol 999") && why.contains("far"), "{why}");

    // What a missing canopy looks like over ssh: stderr explains, pending calls fail.
    let (tx, rx) = mpsc::channel();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 0.3; echo 'bash: canopy: command not found' >&2; exit 127"]);
    let conn = client::connect(cmd, move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let err = answer(|r| conn.handle(0).message("x".into(), r)).unwrap_err();
    assert!(err.contains("command not found"), "{err}");
    let why = closed(&rx);
    assert!(why.contains("hello") && why.contains("command not found"), "{why}");
}
