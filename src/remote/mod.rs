//! Repositories on other machines: `canopy serve` runs the normal workers where the repositories
//! (and their agents) are, and talks to the canopy that shows them over its stdin and stdout,
//! which in practice are an ssh connection (`ssh <host> canopy serve <paths>`). No listening
//! ports: authentication, encryption and NAT are ssh's job.
//!
//! The wire is newline-delimited JSON, one message per line. The server speaks first with
//! [`ToClient::Hello`] (the client ignores anything a login script printed before it), then
//! [`ToClient::Repos`] (what each path argument resolved to), then snapshots, herdr views and
//! replies as they come. The client sends [`ToServer`] requests and calls; closing its end
//! stops the server.
//!
//! A snapshot's graph is sent only when it changed (`graph: None` keeps the previous one): it is
//! by far the biggest part, and most snapshots are status changes.

pub mod client;
pub mod serve;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cleanup::Plan;
use crate::diff::{DiffDoc, Target};
use crate::graph::Graph;
use crate::herdr;
use crate::model::{Snapshot, WorktreeView};
use crate::watch::Request;

/// Wire protocol version, checked in the hello: both sides must speak the same one.
pub const PROTOCOL: u32 = 1;

/// Server → client.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToClient {
    Hello {
        protocol: u32,
        /// The server's build (`canopy --version`).
        version: String,
        /// The server's host name.
        host: String,
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
    /// What the server's herdr reports (`None`: not reachable there, or `--no-herdr`).
    Herdr(Option<Arc<herdr::View>>),
    /// The answer to [`ToServer::Call`] `id`.
    Reply { id: u64, result: Result<Answer, String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub id: u64,
    pub common_dir: PathBuf,
}

/// Client → server.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToServer {
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
    Patch { target: Target, file: Option<String> },
    Message(String),
    CleanupPlan { worktree: Box<WorktreeView>, base: String },
    CleanupRun(Box<Plan>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Answer {
    Doc(DiffDoc),
    Text(String),
    Plan(Box<Plan>),
}

/// Write `msg` as one line and flush it.
pub fn send(out: &mut impl Write, msg: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *out, msg)?;
    out.write_all(b"\n")?;
    out.flush()
}
