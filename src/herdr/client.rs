//! The herdr connection thread: subscribe, read, re-read on events, reconnect on loss.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::json::{self, Json};
use super::{Line, View, classify, parse_agents, parse_workspaces, socket_path, subscribe_request};

/// Per-request timeout on the request connection.
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to keep collecting a burst of events before re-reading once.
const COALESCE: Duration = Duration::from_millis(50);
/// Poll period when subscribing isn't possible.
const POLL: Duration = Duration::from_secs(2);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

type Emit = dyn Fn(Option<Arc<View>>) + Send;

/// Start the herdr thread. `emit` gets every new view, and `None` whenever herdr is unreachable
/// (the UI then hides the feature).
pub fn spawn(emit: impl Fn(Option<Arc<View>>) + Send + 'static) {
    spawn_with(socket_path, emit);
}

/// [`spawn`] with an explicit socket resolver (re-evaluated on every reconnect).
pub fn spawn_with(
    resolve: impl Fn() -> Option<PathBuf> + Send + 'static,
    emit: impl Fn(Option<Arc<View>>) + Send + 'static,
) {
    thread::Builder::new()
        .name("canopy-herdr".into())
        .spawn(move || run(&resolve, &emit))
        .expect("spawn herdr thread");
}

/// Ask herdr to focus the pane of an agent (the one write-like call canopy makes, and only on
/// the user's request). Runs in the background; failures are ignored.
pub fn focus(pane_id: String) {
    let _ = thread::Builder::new().name("canopy-herdr-focus".into()).spawn(move || {
        if let Some(path) = socket_path() {
            let _ = focus_at(&path, &pane_id);
        }
    });
}

/// `agent.focus` for `pane_id` on the server at `path` (blocking).
pub fn focus_at(path: &Path, pane_id: &str) -> std::io::Result<()> {
    let params = format!("{{\"target\":{}}}", json::quote(pane_id));
    Rpc::connect(path)?.call("agent.focus", &params).map(|_| ())
}

fn run(resolve: &dyn Fn() -> Option<PathBuf>, emit: &Emit) {
    let mut backoff = BACKOFF_MIN;
    let mut shown = true; // whether the last emit was a view (so "unreachable" is sent once)
    loop {
        let connected = resolve().is_some_and(|path| session(&path, emit, &mut backoff).is_ok());
        if !connected && shown {
            emit(None);
            shown = false;
        } else if connected {
            shown = true;
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Request/response calls. herdr answers one request per connection and then closes it, so each
/// call opens a fresh connection (cheap on a local socket).
struct Rpc {
    path: PathBuf,
    next_id: u64,
}

impl Rpc {
    fn connect(path: &Path) -> std::io::Result<Rpc> {
        // Fail early (and hide the feature) when nothing listens on the socket.
        UnixStream::connect(path)?;
        Ok(Rpc {
            path: path.to_path_buf(),
            next_id: 0,
        })
    }

    fn call(&mut self, method: &str, params: &str) -> std::io::Result<Json> {
        self.next_id += 1;
        let id = format!("canopy-{}", self.next_id);
        let mut stream = UnixStream::connect(&self.path)?;
        stream.set_read_timeout(Some(RPC_TIMEOUT))?;
        let req = format!(
            "{{\"id\":{},\"method\":{},\"params\":{params}}}\n",
            json::quote(&id),
            json::quote(method)
        );
        stream.write_all(req.as_bytes())?;
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Err(ErrorKind::UnexpectedEof.into());
            }
            let Some(v) = Json::parse(line.trim_end()) else {
                continue;
            };
            if v.get_str("id") != Some(&id) {
                continue;
            }
            if v.get("error").is_some() {
                return Err(std::io::Error::other(line.trim_end().to_string()));
            }
            return Ok(v);
        }
    }

    /// Authoritative read of agents and workspaces.
    fn read_view(&mut self) -> std::io::Result<View> {
        let bad = || std::io::Error::from(ErrorKind::InvalidData);
        let agents = parse_agents(&self.call("agent.list", "{}")?).ok_or_else(bad)?;
        let workspaces = parse_workspaces(&self.call("workspace.list", "{}")?).ok_or_else(bad)?;
        Ok(View { agents, workspaces })
    }
}

fn panes(view: &View) -> BTreeSet<String> {
    view.agents.iter().map(|a| a.pane_id.clone()).collect()
}

/// Open a subscription for lifecycle events and these agent panes. `Ok(None)` if herdr rejected
/// it (e.g. a pane closed in between, or subscriptions unsupported).
fn subscribe(path: &Path, panes: &BTreeSet<String>) -> std::io::Result<Option<BufReader<UnixStream>>> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(RPC_TIMEOUT))?;
    stream.write_all(format!("{}\n", subscribe_request("canopy-sub", panes)).as_bytes())?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        match classify(line.trim_end()) {
            Line::Started => break,
            Line::Error(_) => return Ok(None),
            Line::Event | Line::Other => {}
        }
    }
    // From here on the stream only pushes events: wait for them indefinitely.
    reader.get_ref().set_read_timeout(None)?;
    Ok(Some(reader))
}

/// One connected session. Returns `Ok` when the subscription was lost (`events_lost`, EOF) and
/// the caller should reconnect, `Err` when herdr is unreachable.
fn session(path: &Path, emit: &Emit, backoff: &mut Duration) -> std::io::Result<()> {
    let mut rpc = Rpc::connect(path)?;
    let mut view = rpc.read_view()?;
    emit(Some(Arc::new(view.clone())));
    *backoff = BACKOFF_MIN;
    let mut rejected = 0;
    loop {
        // Subscribe first, then read, so no change between the two goes unnoticed.
        let watched = panes(&view);
        let Some(mut sub) = subscribe(path, &watched)? else {
            rejected += 1;
            if rejected >= 2 {
                return poll(&mut rpc, view, emit);
            }
            view = refresh(&mut rpc, view, emit)?;
            continue;
        };
        rejected = 0;
        view = refresh(&mut rpc, view, emit)?;
        if panes(&view) != watched {
            continue; // an agent appeared or left meanwhile: resubscribe for the new set
        }
        loop {
            match wait_for_events(&mut sub)? {
                Signal::Changed => {}
                Signal::Lost => return Ok(()),
            }
            view = refresh(&mut rpc, view, emit)?;
            if panes(&view) != watched {
                break; // resubscribe for the new set of agent panes
            }
        }
    }
}

enum Signal {
    Changed,
    Lost,
}

/// Block until an event arrives, then drain the rest of the burst. `Lost` on `events_lost`,
/// another error, or EOF.
fn wait_for_events(sub: &mut BufReader<UnixStream>) -> std::io::Result<Signal> {
    let mut line = String::new();
    let mut seen = false;
    loop {
        line.clear();
        match sub.read_line(&mut line) {
            Ok(0) => return Ok(Signal::Lost),
            Ok(_) => match classify(line.trim_end()) {
                Line::Error(_) => return Ok(Signal::Lost),
                Line::Event => {
                    if !seen {
                        seen = true;
                        sub.get_ref().set_read_timeout(Some(COALESCE))?;
                    }
                }
                Line::Started | Line::Other => {}
            },
            Err(e) if seen && matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                sub.get_ref().set_read_timeout(None)?;
                return Ok(Signal::Changed);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Re-read and emit if anything changed.
fn refresh(rpc: &mut Rpc, old: View, emit: &Emit) -> std::io::Result<View> {
    let view = rpc.read_view()?;
    if view != old {
        emit(Some(Arc::new(view.clone())));
    }
    Ok(view)
}

/// Fallback when subscriptions don't work: re-read every [`POLL`].
fn poll(rpc: &mut Rpc, mut view: View, emit: &Emit) -> std::io::Result<()> {
    loop {
        thread::sleep(POLL);
        view = refresh(rpc, view, emit)?;
    }
}
