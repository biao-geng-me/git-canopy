//! The herdr client against a fake herdr server on a temporary Unix socket: initial read,
//! re-read on a pushed event, resubscribe after `events_lost`, focus, the shell pane, and hiding
//! when herdr goes away.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use canopy::herdr::{self, AgentState, View, json::Json};

#[derive(Default)]
struct Fake {
    /// Status the next `agent.list` reports.
    status: Mutex<String>,
    subscribes: AtomicUsize,
    /// Open subscription streams (to push events into).
    subs: Mutex<Vec<UnixStream>>,
    /// Every method called, in order.
    calls: Mutex<Vec<String>>,
}

fn agent_list(id: &str, status: &str) -> String {
    format!(
        r#"{{"id":"{id}","result":{{"type":"agent_list","agents":[{{"terminal_id":"t1","agent":"claude","agent_status":"{status}","cwd":"/w/repo","foreground_cwd":"/w/repo/src","focused":false,"name":"dev","pane_id":"w1:p1","revision":1,"tab_id":"w1:t1","workspace_id":"w1"}}]}}}}"#
    )
}

fn workspace_list(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","result":{{"type":"workspace_list","workspaces":[{{"workspace_id":"w1","number":1,"label":"repo","focused":true,"pane_count":1,"tab_count":1,"active_tab_id":"w1:t1","agent_status":"working","worktree":null}}]}}}}"#
    )
}

fn serve(listener: UnixListener, fake: Arc<Fake>) {
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { break };
        let fake = fake.clone();
        thread::spawn(move || {
            let mut line = String::new();
            if BufReader::new(conn.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap_or(0)
                == 0
            {
                return;
            }
            let req = Json::parse(line.trim()).unwrap();
            let id = req.get_str("id").unwrap().to_string();
            let method = req.get_str("method").unwrap().to_string();
            fake.calls.lock().unwrap().push(method.clone());
            match method.as_str() {
                "agent.list" => {
                    let status = fake.status.lock().unwrap().clone();
                    let _ = writeln!(conn, "{}", agent_list(&id, &status));
                }
                "workspace.list" => {
                    let _ = writeln!(conn, "{}", workspace_list(&id));
                }
                "events.subscribe" => {
                    let subs = req.get("params").unwrap().get("subscriptions").unwrap().arr().unwrap();
                    assert!(
                        subs.iter()
                            .any(|s| s.get_str("type") == Some("pane.agent_status_changed")
                                && s.get_str("pane_id") == Some("w1:p1"))
                    );
                    fake.subscribes.fetch_add(1, Ordering::SeqCst);
                    let _ = writeln!(conn, r#"{{"id":"{id}","result":{{"type":"subscription_started"}}}}"#);
                    fake.subs.lock().unwrap().push(conn); // keep it open for pushed events
                }
                "pane.split" => {
                    let params = req.get("params").unwrap();
                    fake.calls.lock().unwrap().push(format!(
                        "split {} {} {}",
                        params.get_str("target_pane_id").unwrap(),
                        params.get_str("direction").unwrap(),
                        params.get_str("cwd").unwrap()
                    ));
                    let _ = writeln!(conn, r#"{{"id":"{id}","result":{{"type":"pane_info"}}}}"#);
                }
                "agent.focus" => {
                    let target = req.get("params").unwrap().get_str("target").unwrap().to_string();
                    fake.calls.lock().unwrap().push(format!("focus {target}"));
                    let _ = writeln!(conn, r#"{{"id":"{id}","result":{{"type":"ok"}}}}"#);
                }
                other => panic!("unexpected call: {other}"),
            }
        });
    }
}

fn wait_view(rx: &mpsc::Receiver<Option<Arc<View>>>, pred: impl Fn(&Option<Arc<View>>) -> bool) -> Option<Arc<View>> {
    loop {
        let v = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out waiting for a herdr update");
        if pred(&v) {
            return v;
        }
    }
}

fn until(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("condition not reached");
}

#[test]
fn follows_events_and_recovers_from_events_lost() {
    // Unix socket paths are limited to ~108 bytes: fall back to /tmp for long temp dirs.
    let base = Some(std::env::temp_dir())
        .filter(|t| t.as_os_str().len() < 60)
        .unwrap_or_else(|| "/tmp".into());
    let dir = base.join(format!("canopy-herdr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("herdr.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let fake = Arc::new(Fake::default());
    *fake.status.lock().unwrap() = "working".into();
    let f = fake.clone();
    thread::spawn(move || serve(listener, f));

    let (tx, rx) = mpsc::channel();
    let path: PathBuf = sock.clone();
    herdr::spawn_with(
        move || Some(path.clone()),
        move |v| {
            let _ = tx.send(v);
        },
    );

    // Initial read.
    let v = wait_view(&rx, |v| v.is_some()).unwrap();
    assert_eq!(v.agents[0].state, AgentState::Working);
    let wts = [PathBuf::from("/w/repo")];
    assert_eq!(herdr::assign(&v, &wts)[0], [0]);
    until(|| !fake.subs.lock().unwrap().is_empty());

    // A pushed status event is a signal to re-read.
    *fake.status.lock().unwrap() = "blocked".into();
    {
        let mut subs = fake.subs.lock().unwrap();
        writeln!(
            subs[0],
            r#"{{"event":"pane.agent_status_changed","data":{{"pane_id":"w1:p1","workspace_id":"w1","agent_status":"blocked"}}}}"#
        )
        .unwrap();
    }
    let v = wait_view(&rx, |v| {
        v.as_ref().is_some_and(|v| v.agents[0].state == AgentState::Blocked)
    })
    .unwrap();
    assert_eq!(herdr::activity(&v, &[0]).unwrap().state, AgentState::Blocked);

    // events_lost: herdr reports it and closes the subscription; the client resubscribes and
    // reconciles with a fresh read.
    *fake.status.lock().unwrap() = "done".into();
    {
        let mut subs = fake.subs.lock().unwrap();
        let mut s = subs.remove(0);
        writeln!(
            s,
            r#"{{"id":"canopy-sub","error":{{"code":"events_lost","message":"subscriber fell behind"}}}}"#
        )
        .unwrap();
    }
    wait_view(&rx, |v| {
        v.as_ref().is_some_and(|v| v.agents[0].state == AgentState::Done)
    });
    until(|| fake.subscribes.load(Ordering::SeqCst) >= 2);

    // `a` in the UI: the only write-like call.
    herdr::focus_at(&sock, "w1:p1").unwrap();
    assert!(fake.calls.lock().unwrap().iter().any(|c| c == "focus w1:p1"));

    // `s` in the UI: a shell in a pane split below canopy's.
    herdr::open_shell_pane_at(&sock, "w1:p9", std::path::Path::new("/w/repo")).unwrap();
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "split w1:p9 down /w/repo")
    );

    // herdr goes away: the feature hides.
    std::fs::remove_file(&sock).unwrap();
    fake.subs.lock().unwrap().clear();
    wait_view(&rx, |v| v.is_none());

    // Apart from the explicit focus and shell pane, only read-only methods were called.
    let calls = fake.calls.lock().unwrap();
    let allowed = [
        "agent.list",
        "workspace.list",
        "events.subscribe",
        "agent.focus",
        "focus w1:p1",
        "pane.split",
        "split w1:p9 down /w/repo",
    ];
    assert!(calls.iter().all(|c| allowed.contains(&c.as_str())), "{calls:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
