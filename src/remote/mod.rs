//! Repositories on other machines: `canopy serve` runs the normal workers where the repositories
//! (and their agents) are, and talks to the canopy that shows them over its stdin and stdout,
//! which in practice are an ssh connection (`ssh <host> canopy serve <paths>`). No listening
//! ports: authentication, encryption and NAT are ssh's job.
//!
//! The wire is newline-delimited JSON, one message per line. The server speaks first with
//! [`ToClient::Hello`] (the client ignores anything a login script printed before it), then
//! [`ToClient::Repos`] (what each path argument resolved to). Nothing is watched until the
//! client says how with [`ToServer::Watch`] per repository: in full (snapshots), or only its
//! file events ([`ToClient::Touched`]), for a repository on a filesystem shared with a machine
//! that already watches it in full. Then snapshots, events, herdr views and replies come as they
//! happen. The client sends [`ToServer`] requests and calls; closing its end stops the server.
//!
//! A snapshot's graph is sent only when it changed (`graph: None` keeps the previous one): it is
//! by far the biggest part, and most snapshots are status changes.

pub mod client;
pub mod install;
pub mod serve;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cleanup::{self, Plan};
use crate::diff::{DiffDoc, Target};
use crate::graph::Graph;
use crate::herdr;
use crate::model::Snapshot;
use crate::watch::Request;

/// Wire protocol version, checked in the hello: both sides must speak the same one.
pub const PROTOCOL: u32 = 3;

/// Server → client.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToClient {
    Hello {
        protocol: u32,
        /// The server's build (`canopy --version`).
        version: String,
        /// The server's host name.
        host: String,
        /// Where the server's herdr creates worktrees (`herdr::worktree_dir`; `None` with
        /// `--no-herdr`), for the "herdr made it but can't see it" cleanup guard.
        herdr_dir: Option<PathBuf>,
    },
    /// What each path argument resolved to, in order (two paths in one repository get the same
    /// id).
    Repos(Vec<Result<RepoInfo, String>>),
    Snapshot {
        repo: u64,
        snap: Arc<Snapshot>,
        /// The snapshot's graph, `None` when it is the one sent last for this repository.
        graph: Option<Arc<Graph>>,
    },
    /// File events of a repository watched with [`Mode::Events`] (for `watch::Request::Touched`).
    Touched { repo: u64, paths: Vec<PathBuf> },
    /// What the server's herdr reports (`None`: not reachable there, or `--no-herdr`).
    Herdr(Option<Arc<herdr::View>>),
    /// The answer to [`ToServer::Call`] `id`.
    Reply { id: u64, result: Result<Answer, String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub id: u64,
    pub common_dir: PathBuf,
    /// `git::identity`: equal for the same repository seen from two machines.
    pub identity: Option<String>,
}

/// How the server watches a repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// A worker and its snapshots, as locally.
    Full,
    /// Only file events, forwarded ([`ToClient::Touched`]): another machine refreshes it.
    Events,
}

/// Client → server.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToServer {
    /// Which repositories these directories are in (`--herdr-repos`: where the server's herdr
    /// agents work). Each found repository gets an id, unwatched until [`ToServer::Watch`].
    /// Answered with a [`ToClient::Reply`] carrying [`Answer::Found`].
    Discover {
        id: u64,
        dirs: Vec<PathBuf>,
    },
    /// Stop watching repository `repo` (a herdr-found one nobody works in anymore).
    Unwatch {
        repo: u64,
    },
    /// Start (or switch) watching repository `repo` this way.
    Watch {
        repo: u64,
        mode: Mode,
    },
    Request {
        repo: u64,
        req: Request,
    },
    /// Answered with a [`ToClient::Reply`] carrying the same `id`.
    Call {
        id: u64,
        repo: u64,
        call: Call,
    },
}

/// The requests a `watch::Handle` answers, as data.
#[derive(Debug, Serialize, Deserialize)]
pub enum Call {
    Doc(Target),
    Patch {
        target: Target,
        file: Option<String>,
    },
    Message(String),
    CleanupPlan {
        target: cleanup::Target,
        base: Option<String>,
    },
    CleanupRun {
        plan: Box<Plan>,
        chosen: Vec<bool>,
    },
    /// Ask the server's herdr to focus this agent pane (`a`); answered with empty text.
    FocusAgent(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Answer {
    Doc(DiffDoc),
    Text(String),
    Plan(Box<Plan>),
    /// For [`ToServer::Discover`]: each directory's repository (`None`: not in one).
    Found(Vec<(PathBuf, Option<RepoInfo>)>),
}

/// Write `msg` as one line and flush it.
pub fn send(out: &mut impl Write, msg: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *out, msg)?;
    out.write_all(b"\n")?;
    out.flush()
}

/// This machine's host name.
pub fn hostname() -> String {
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
