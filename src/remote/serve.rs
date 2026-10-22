//! `canopy serve`: watch repositories on this machine for a canopy elsewhere, over stdin/stdout.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::{Context, Result};

use super::{Answer, Call, Mode, PROTOCOL, RepoInfo, ToClient, ToServer, send};
use crate::git::{self, RepoPaths};
use crate::graph::Graph;
use crate::herdr;
use crate::model::Snapshot;
use crate::watch::{self, Calls, Config, Handle, Hub, Reply};

/// What the writer thread sends; snapshots go through it to drop graphs the client already has.
enum Out {
    Snapshot(u64, Arc<Snapshot>),
    Msg(Box<ToClient>),
}

/// Serve the repositories containing `paths` until the client closes stdin. `herdr`: also
/// report this machine's herdr agents.
pub fn run(paths: Vec<PathBuf>, cfg: Config, with_herdr: bool) -> Result<()> {
    let (input, output) = take_stdio().context("taking over stdin/stdout")?;
    let (tx, rx) = mpsc::channel::<Out>();
    thread::Builder::new()
        .name("canopy-serve-out".into())
        .spawn(move || write_loop(output, rx))
        .context("spawn writer")?;
    let _ = tx.send(Out::Msg(Box::new(ToClient::Hello {
        protocol: PROTOCOL,
        version: env!("CANOPY_BUILD").to_string(),
        host: super::hostname(),
        herdr_dir: if with_herdr { herdr::worktree_dir() } else { None },
    })));

    // Resolve every path first, so the client knows the ids before the first snapshot.
    // Nothing is watched until the client's `Watch`, which knows whether another machine
    // watches it already.
    let hub = Hub::new();
    let mut served: Vec<Served> = Vec::new();
    let answers: Vec<Result<RepoInfo, String>> = paths
        .iter()
        .map(|p| resolve(&mut served, p, &cfg).map_err(|e| format!("{}: {e:#}", p.display())))
        .collect();
    let _ = tx.send(Out::Msg(Box::new(ToClient::Repos(answers))));
    if with_herdr {
        let tx = tx.clone();
        herdr::spawn(move |view| {
            let _ = tx.send(Out::Msg(Box::new(ToClient::Herdr(view))));
        });
    }

    for line in BufReader::new(input).lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: ToServer = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                // stderr reaches the client's error report; one bad line shouldn't end the rest.
                eprintln!("canopy serve: bad request ({e}): {line}");
                continue;
            }
        };
        match msg {
            ToServer::Discover { id, dirs } => {
                let found = dirs
                    .into_iter()
                    .map(|d| {
                        let info = resolve(&mut served, &d, &cfg).ok();
                        (d, info)
                    })
                    .collect();
                let result = Ok(Answer::Found(found));
                let _ = tx.send(Out::Msg(Box::new(ToClient::Reply { id, result })));
            }
            ToServer::Unwatch { repo } => {
                if let Some(s) = served.get_mut(repo as usize) {
                    s.worker = None;
                }
            }
            ToServer::Watch { repo, mode } => {
                let Some(s) = served.get_mut(repo as usize) else {
                    continue;
                };
                if s.worker.as_ref().is_some_and(|(m, _)| *m == mode) {
                    continue;
                }
                // Stop the old way first: never two workers on one repository.
                s.worker = None;
                let (paths, cfg, tx) = (s.paths.clone(), cfg.clone(), tx.clone());
                let handle = match mode {
                    Mode::Full => watch::spawn_with(paths, cfg, hub.clone(), move |snap| {
                        let _ = tx.send(Out::Snapshot(repo, snap));
                    }),
                    Mode::Events => watch::spawn_events(paths, cfg, hub.clone(), move |paths| {
                        let _ = tx.send(Out::Msg(Box::new(ToClient::Touched { repo, paths })));
                    }),
                };
                s.worker = Some((mode, handle));
            }
            ToServer::Request { repo, req } => {
                if let Some((_, h)) = served.get(repo as usize).and_then(|s| s.worker.as_ref()) {
                    h.request(req);
                }
            }
            ToServer::Call { id, repo, call } => match served.get(repo as usize) {
                Some(s) => dispatch(&s.calls, call, id, &tx),
                None => {
                    let result = Err(format!("no repository {repo}"));
                    let _ = tx.send(Out::Msg(Box::new(ToClient::Reply { id, result })));
                }
            },
        }
    }
    // The client is gone: dropping the handles stops the workers.
    Ok(())
}

/// The repository containing `path`, by its id (a new one listed last, unwatched).
fn resolve(served: &mut Vec<Served>, path: &Path, cfg: &Config) -> Result<RepoInfo> {
    let found = git::discover(path)?;
    let id = match served.iter().position(|s| s.paths.common_dir == found.common_dir) {
        Some(i) => i,
        None => {
            served.push(Served {
                calls: Calls::new(found.common_dir.clone(), cfg.herdr),
                paths: found.clone(),
                worker: None,
            });
            served.len() - 1
        }
    } as u64;
    Ok(RepoInfo {
        id,
        identity: git::identity(&found.common_dir),
        common_dir: found.common_dir,
    })
}

/// One repository being served.
struct Served {
    paths: RepoPaths,
    calls: Calls,
    /// How it is watched, once the client said (dropping the handle stops the worker).
    worker: Option<(Mode, Handle)>,
}

fn dispatch(h: &Calls, call: Call, id: u64, tx: &Sender<Out>) {
    fn reply<T: 'static>(tx: &Sender<Out>, id: u64, wrap: fn(T) -> Answer) -> Reply<T> {
        let tx = tx.clone();
        Box::new(move |r: Result<T, String>| {
            let result = r.map(wrap);
            let _ = tx.send(Out::Msg(Box::new(ToClient::Reply { id, result })));
        })
    }
    match call {
        Call::Doc(target) => h.doc(target, reply(tx, id, Answer::Doc)),
        Call::Patch { target, file } => h.patch(target, file, reply(tx, id, Answer::Text)),
        Call::Message(sha) => h.message(sha, reply(tx, id, Answer::Text)),
        Call::CleanupPlan { worktree, base } => {
            h.cleanup_plan(*worktree, base, reply(tx, id, |p| Answer::Plan(Box::new(p))))
        }
        Call::CleanupRun(plan) => h.cleanup_run(*plan, reply(tx, id, Answer::Text)),
        Call::FocusAgent(pane) => {
            herdr::focus(pane);
            reply(tx, id, Answer::Text)(Ok(String::new()));
        }
    }
}

fn write_loop(output: File, rx: mpsc::Receiver<Out>) {
    let mut out = BufWriter::new(output);
    let mut sent: HashMap<u64, Arc<Graph>> = HashMap::new();
    for msg in rx {
        let msg = match msg {
            Out::Msg(m) => *m,
            Out::Snapshot(repo, snap) => {
                let graph = match sent.get(&repo) {
                    Some(g) if Arc::ptr_eq(g, &snap.graph) => None,
                    _ => {
                        sent.insert(repo, snap.graph.clone());
                        Some(snap.graph.clone())
                    }
                };
                ToClient::Snapshot { repo, snap, graph }
            }
        };
        if send(&mut out, &msg).is_err() {
            // The client is gone; reading stdin ends too.
            return;
        }
    }
}

/// The protocol's stdin and stdout, moved to private descriptors: fd 0 becomes `/dev/null` and
/// fd 1 goes to stderr, so nothing else (a child process that inherits them, a stray print)
/// can read requests or corrupt the stream.
fn take_stdio() -> std::io::Result<(File, File)> {
    // SAFETY: plain descriptor juggling on fds 0-2, before any other thread uses them; the new
    // descriptors are owned by the returned Files only.
    unsafe {
        let input = libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3);
        let output = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if input < 0 || output < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if null < 0 || libc::dup2(null, 0) < 0 || libc::dup2(2, 1) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        libc::close(null);
        Ok((File::from_raw_fd(input), File::from_raw_fd(output)))
    }
}
