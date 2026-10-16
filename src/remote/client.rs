//! The other end of `canopy serve`: start it (over ssh, or any command speaking the protocol on
//! its stdin/stdout), read what it sends, and answer `watch::Handle`'s requests through it.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Read};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::{Answer, Call, PROTOCOL, RepoInfo, ToClient, ToServer, send};
use crate::cleanup::Plan;
use crate::diff::{DiffDoc, Target};
use crate::graph::Graph;
use crate::herdr;
use crate::model::{Snapshot, WorktreeView};
use crate::watch::{Config, Reply, Request};

/// Lines a login script may print before the hello.
const MAX_PREAMBLE: usize = 200;
/// Last lines of the server's stderr kept for the error report.
const STDERR_TAIL: usize = 20;

/// What the server sends, as it arrives (on the connection's reader thread).
#[derive(Debug)]
pub enum Event {
    /// The server's build and host name.
    Hello {
        version: String,
        host: String,
    },
    /// What each path argument resolved to, in order.
    Repos(Vec<Result<RepoInfo, String>>),
    /// A snapshot of repository `.0`, its graph filled in.
    Snapshot(u64, Arc<Snapshot>),
    Herdr(Option<Arc<herdr::View>>),
    /// The connection is over (the last event): why, including what the server printed to
    /// stderr. Replies still pending get the same as an error.
    Closed(String),
}

type Pending = Box<dyn FnOnce(Result<Answer, String>) + Send>;
/// Calls waiting for their reply; `None` once the connection is closed.
type PendingMap = Arc<Mutex<Option<HashMap<u64, Pending>>>>;

/// A running server. Cheap to clone; when the last clone goes, its stdin is closed, which stops
/// the server.
#[derive(Clone)]
pub struct Conn(Arc<Inner>);

struct Inner {
    stdin: Mutex<BufWriter<ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicU64,
    child: Mutex<Option<Child>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Closing stdin (when `stdin` drops after this) ends the server; reap it off this thread.
        if let Some(mut child) = self.child.lock().unwrap().take() {
            thread::spawn(move || {
                for _ in 0..50 {
                    if !matches!(child.try_wait(), Ok(None)) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

/// Start `cmd` (which must run `canopy serve`) and read from it on a thread of its own; every
/// [`Event`] goes to `emit`, [`Event::Closed`] last.
pub fn connect(mut cmd: Command, emit: impl Fn(Event) + Send + 'static) -> std::io::Result<Conn> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let err_reader = {
        let tail = tail.clone();
        thread::Builder::new().name("canopy-remote-err".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = tail.lock().unwrap();
                if t.len() == STDERR_TAIL {
                    t.pop_front();
                }
                t.push_back(line);
            }
        })?
    };
    let pending: PendingMap = Arc::new(Mutex::new(Some(HashMap::new())));
    {
        let pending = pending.clone();
        thread::Builder::new().name("canopy-remote".into()).spawn(move || {
            let reason = read_loop(stdout, &pending, &emit);
            // What the server said on its way out explains more than "closed".
            let _ = err_reader.join();
            let tail: Vec<String> = tail.lock().unwrap().drain(..).collect();
            let reason = if tail.is_empty() {
                reason
            } else {
                format!("{reason}: {}", tail.join(" / "))
            };
            if let Some(waiting) = pending.lock().unwrap().take() {
                for (_, reply) in waiting {
                    reply(Err(reason.clone()));
                }
            }
            emit(Event::Closed(reason));
        })?;
    }
    Ok(Conn(Arc::new(Inner {
        stdin: Mutex::new(BufWriter::new(stdin)),
        pending,
        next_id: AtomicU64::new(0),
        child: Mutex::new(Some(child)),
    })))
}

/// Read messages until the end; returns why it ended.
fn read_loop(stdout: impl Read, pending: &PendingMap, emit: &dyn Fn(Event)) -> String {
    let mut lines = BufReader::new(stdout).lines();
    // The hello, after whatever a login script printed.
    let mut skipped = 0;
    loop {
        let Some(Ok(line)) = lines.next() else {
            return "the server ended before saying hello".into();
        };
        match serde_json::from_str::<ToClient>(&line) {
            Ok(ToClient::Hello {
                protocol,
                version,
                host,
            }) => {
                if protocol != PROTOCOL {
                    return format!(
                        "canopy {version} on {host} speaks protocol {protocol}, this canopy {PROTOCOL}: \
                         install the same version on both"
                    );
                }
                emit(Event::Hello { version, host });
                break;
            }
            Ok(_) => return "the server didn't say hello first".into(),
            Err(_) if skipped < MAX_PREAMBLE => skipped += 1,
            Err(_) => return "no canopy hello in the server's output".into(),
        }
    }

    // Graph per repository, for snapshots that keep the last one.
    let mut graphs: HashMap<u64, Arc<Graph>> = HashMap::new();
    for line in lines {
        let line = match line {
            Ok(l) => l,
            Err(e) => return format!("reading from the server: {e}"),
        };
        let msg = match serde_json::from_str::<ToClient>(&line) {
            Ok(m) => m,
            Err(e) => return format!("bad message from the server: {e}"),
        };
        match msg {
            ToClient::Hello { .. } => return "the server said hello twice".into(),
            ToClient::Repos(r) => emit(Event::Repos(r)),
            ToClient::Snapshot { repo, mut snap, graph } => {
                let graph = match graph {
                    Some(g) => {
                        graphs.insert(repo, g.clone());
                        g
                    }
                    None => graphs.get(&repo).cloned().unwrap_or_default(),
                };
                Arc::make_mut(&mut snap).graph = graph;
                emit(Event::Snapshot(repo, snap));
            }
            ToClient::Herdr(view) => emit(Event::Herdr(view)),
            ToClient::Reply { id, result } => {
                let reply = pending.lock().unwrap().as_mut().and_then(|p| p.remove(&id));
                if let Some(reply) = reply {
                    reply(result);
                }
            }
        }
    }
    "the connection closed".into()
}

impl Conn {
    pub fn request(&self, repo: u64, req: Request) {
        // A dead connection reports itself with `Event::Closed`.
        let _ = send(&mut *self.0.stdin.lock().unwrap(), &ToServer::Request { repo, req });
    }

    /// Send `call` for repository `repo`; `reply` gets the answer, or the reason there won't be
    /// one.
    fn call(&self, repo: u64, call: Call, reply: Pending) {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        match self.0.pending.lock().unwrap().as_mut() {
            Some(p) => p.insert(id, reply),
            None => return reply(Err("the connection is closed".into())),
        };
        let sent = send(&mut *self.0.stdin.lock().unwrap(), &ToServer::Call { id, repo, call });
        if let Err(e) = sent {
            let reply = self.0.pending.lock().unwrap().as_mut().and_then(|p| p.remove(&id));
            if let Some(reply) = reply {
                reply(Err(format!("sending to the server: {e}")));
            }
        }
    }

    /// Requests for repository `repo` of this server, as a `watch::Handle` takes them.
    pub fn handle(&self, repo: u64) -> RemoteHandle {
        RemoteHandle {
            conn: self.clone(),
            repo,
        }
    }
}

/// One repository on a server: the same requests as a local `watch::Handle`.
#[derive(Clone)]
pub struct RemoteHandle {
    conn: Conn,
    repo: u64,
}

/// `reply` for an answer that should be `want`.
fn expect<T: 'static>(reply: Reply<T>, want: fn(Answer) -> Option<T>) -> Pending {
    Box::new(move |r| reply(r.and_then(|a| want(a).ok_or_else(|| "unexpected answer from the server".to_string()))))
}

fn doc(a: Answer) -> Option<DiffDoc> {
    match a {
        Answer::Doc(d) => Some(d),
        _ => None,
    }
}

fn text(a: Answer) -> Option<String> {
    match a {
        Answer::Text(t) => Some(t),
        _ => None,
    }
}

fn plan(a: Answer) -> Option<Plan> {
    match a {
        Answer::Plan(p) => Some(*p),
        _ => None,
    }
}

impl RemoteHandle {
    pub fn request(&self, req: Request) {
        self.conn.request(self.repo, req);
    }

    pub fn doc(&self, target: Target, reply: Reply<DiffDoc>) {
        self.conn.call(self.repo, Call::Doc(target), expect(reply, doc));
    }

    pub fn patch(&self, target: Target, file: Option<String>, reply: Reply<String>) {
        self.conn
            .call(self.repo, Call::Patch { target, file }, expect(reply, text));
    }

    pub fn message(&self, sha: String, reply: Reply<String>) {
        self.conn.call(self.repo, Call::Message(sha), expect(reply, text));
    }

    pub fn cleanup_plan(&self, worktree: WorktreeView, base: String, reply: Reply<Plan>) {
        self.conn.call(
            self.repo,
            Call::CleanupPlan {
                worktree: Box::new(worktree),
                base,
            },
            expect(reply, plan),
        );
    }

    pub fn cleanup_run(&self, plan: Plan, reply: Reply<String>) {
        self.conn
            .call(self.repo, Call::CleanupRun(Box::new(plan)), expect(reply, text));
    }
}

/// The arguments for `canopy serve` with `cfg`'s settings (`program` is the remote canopy).
pub fn serve_args(program: &str, cfg: &Config, herdr: bool, paths: &[String]) -> Vec<String> {
    let mut args = vec![program.to_string(), "serve".to_string()];
    args.push(format!("--interval={}", cfg.interval.as_secs_f64()));
    if !cfg.poll {
        args.push("--no-poll".into());
    }
    if !cfg.watch {
        args.push("--no-watch".into());
    }
    if let Some(b) = &cfg.base {
        args.push(format!("--base={b}"));
    }
    if !herdr {
        args.push("--no-herdr".into());
    }
    args.push("--".into());
    args.extend(paths.iter().cloned());
    args
}

/// `ssh host <args>`, never prompting (an unreachable or password-only host fails instead), with
/// keepalives so a dead connection is noticed.
pub fn ssh(host: &str, args: &[String]) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ]);
    // The remote side joins the arguments into one shell command line.
    cmd.arg("--")
        .arg(host)
        .arg(args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
    cmd
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/home/me/proj"), "/home/me/proj");
        assert_eq!(shell_quote("--base=main"), "--base=main");
        assert_eq!(shell_quote("my dir"), "'my dir'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn serve_arguments() {
        let cfg = Config {
            poll: false,
            base: Some("dev".into()),
            ..Config::default()
        };
        assert_eq!(
            serve_args("canopy", &cfg, false, &["/r/a".into()]),
            [
                "canopy",
                "serve",
                "--interval=60",
                "--no-poll",
                "--base=dev",
                "--no-herdr",
                "--",
                "/r/a"
            ]
        );
    }
}
