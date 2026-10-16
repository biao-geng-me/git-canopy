//! `canopy serve`: watch repositories on this machine for a canopy elsewhere, over stdin/stdout.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter};
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::{Context, Result};

use super::{Answer, Call, PROTOCOL, RepoInfo, ToClient, ToServer, send};
use crate::git::{self, RepoPaths};
use crate::graph::Graph;
use crate::herdr;
use crate::model::Snapshot;
use crate::watch::{self, Config, Handle, Hub, Reply};

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
        host: hostname(),
    })));

    // Resolve every path first, so the client knows the ids before the first snapshot.
    let mut repos: Vec<RepoPaths> = Vec::new();
    let answers: Vec<Result<RepoInfo, String>> = paths
        .iter()
        .map(|p| {
            let found = git::discover(p).map_err(|e| format!("{}: {e:#}", p.display()))?;
            let id = match repos.iter().position(|r| r.common_dir == found.common_dir) {
                Some(i) => i,
                None => {
                    repos.push(found.clone());
                    repos.len() - 1
                }
            } as u64;
            Ok(RepoInfo {
                id,
                common_dir: found.common_dir,
            })
        })
        .collect();
    let _ = tx.send(Out::Msg(Box::new(ToClient::Repos(answers))));

    let hub = Hub::new();
    let handles: Vec<Handle> = repos
        .into_iter()
        .enumerate()
        .map(|(id, paths)| {
            let tx = tx.clone();
            watch::spawn_with(paths, cfg.clone(), hub.clone(), move |s| {
                let _ = tx.send(Out::Snapshot(id as u64, s));
            })
        })
        .collect();
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
            ToServer::Request { repo, req } => {
                if let Some(h) = handles.get(repo as usize) {
                    h.request(req);
                }
            }
            ToServer::Call { id, repo, call } => match handles.get(repo as usize) {
                Some(h) => dispatch(h, call, id, &tx),
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

fn dispatch(h: &Handle, call: Call, id: u64, tx: &Sender<Out>) {
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

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates within it or
    // truncates, and the last byte stays 0 either way.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    if ok {
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    }
}
