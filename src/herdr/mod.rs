//! herdr agent activity: which worktrees have a coding agent working, done or blocked on the user.
//!
//! Talks to the herdr server over its Unix socket (newline-delimited JSON). One connection makes
//! requests (`agent.list`, `workspace.list`); a second one holds an `events.subscribe` stream whose
//! events are only used as signals to re-read. Everything is read-only except [`focus`],
//! [`open_shell_pane`] and [`remove_worktree_at`], which the user triggers explicitly. When herdr
//! isn't reachable the feature stays hidden.

pub mod json;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use json::Json;

/// An agent's lifecycle state, ordered by urgency (most urgent last).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AgentState {
    Unknown,
    Idle,
    Done,
    Working,
    /// Waiting at an approval or question: needs the user.
    Blocked,
}

impl AgentState {
    pub fn parse(s: &str) -> AgentState {
        match s {
            "idle" => AgentState::Idle,
            "done" => AgentState::Done,
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            _ => AgentState::Unknown,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            AgentState::Unknown => "agent",
            AgentState::Idle => "idle",
            AgentState::Done => "done",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub pane_id: String,
    pub workspace_id: String,
    /// Agent name if set, else its kind (`claude`, `codex`, …).
    pub label: String,
    pub state: AgentState,
    /// `foreground_cwd`, else `cwd` (canonicalized when it exists).
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub label: String,
    pub panes: usize,
    /// The worktree the herdr workspace belongs to, if any (canonicalized when it exists).
    pub checkout: Option<PathBuf>,
}

/// What herdr currently reports.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub agents: Vec<Agent>,
    pub workspaces: Vec<Workspace>,
}

fn path_of(s: Option<&str>) -> Option<PathBuf> {
    let p = Path::new(s.filter(|s| !s.is_empty())?);
    Some(std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
}

/// Agents from an `agent.list` response (`{"id":…,"result":{"type":"agent_list","agents":[…]}}`).
pub fn parse_agents(resp: &Json) -> Option<Vec<Agent>> {
    let result = resp.get("result")?;
    (result.get_str("type")? == "agent_list").then_some(())?;
    Some(
        result
            .get("agents")?
            .arr()?
            .iter()
            .filter_map(|a| {
                Some(Agent {
                    pane_id: a.get_str("pane_id")?.to_string(),
                    workspace_id: a.get_str("workspace_id").unwrap_or_default().to_string(),
                    label: a
                        .get_str("name")
                        .or_else(|| a.get_str("display_agent"))
                        .or_else(|| a.get_str("agent"))
                        .unwrap_or("agent")
                        .to_string(),
                    state: AgentState::parse(a.get_str("agent_status").unwrap_or_default()),
                    cwd: path_of(a.get_str("foreground_cwd")).or_else(|| path_of(a.get_str("cwd"))),
                })
            })
            .collect(),
    )
}

/// Workspaces from a `workspace.list` response.
pub fn parse_workspaces(resp: &Json) -> Option<Vec<Workspace>> {
    let result = resp.get("result")?;
    (result.get_str("type")? == "workspace_list").then_some(())?;
    Some(
        result
            .get("workspaces")?
            .arr()?
            .iter()
            .filter_map(|w| {
                Some(Workspace {
                    id: w.get_str("workspace_id")?.to_string(),
                    label: w.get_str("label").unwrap_or_default().to_string(),
                    panes: w.get("pane_count").and_then(Json::num).unwrap_or(0.0) as usize,
                    checkout: path_of(w.get("worktree").and_then(|t| t.get_str("checkout_path"))),
                })
            })
            .collect(),
    )
}

/// The deepest of `worktrees` containing `path`.
fn deepest(path: &Path, worktrees: &[PathBuf]) -> Option<usize> {
    worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| path.starts_with(w))
        .max_by_key(|(_, w)| w.components().count())
        .map(|(i, _)| i)
}

/// For each worktree (by index into `worktrees`, canonical paths), the indexes of its agents: by
/// longest path-prefix match of the agent's cwd, else of its workspace's checkout path. Agents
/// outside every worktree are left out.
pub fn assign(view: &View, worktrees: &[PathBuf]) -> Vec<Vec<usize>> {
    let mut out = vec![Vec::new(); worktrees.len()];
    for (ai, a) in view.agents.iter().enumerate() {
        let by_cwd = a.cwd.as_deref().and_then(|c| deepest(c, worktrees));
        let by_workspace = || {
            let ws = view.workspaces.iter().find(|w| w.id == a.workspace_id)?;
            deepest(ws.checkout.as_deref()?, worktrees)
        };
        if let Some(wi) = by_cwd.or_else(by_workspace) {
            out[wi].push(ai);
        }
    }
    out
}

/// Every directory herdr places work in: agents' cwds and workspaces' checkouts (for finding
/// the repositories to watch).
pub fn dirs(view: &View) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let all = view.agents.iter().filter_map(|a| a.cwd.as_ref());
    for p in all.chain(view.workspaces.iter().filter_map(|w| w.checkout.as_ref())) {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// Summary of the agents in one worktree: the most urgent state and how many there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activity {
    pub state: AgentState,
    pub count: usize,
}

pub fn activity(view: &View, agents: &[usize]) -> Option<Activity> {
    let state = agents.iter().map(|&i| view.agents[i].state).max()?;
    Some(Activity {
        state,
        count: agents.len(),
    })
}

/// What a line on a subscription connection is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    /// `subscription_started` acknowledgement.
    Started,
    /// A pushed event (any kind): a signal to re-read.
    Event,
    /// An error response, e.g. `events_lost` (herdr closes the connection after it).
    Error(String),
    Other,
}

pub fn classify(line: &str) -> Line {
    let Some(v) = Json::parse(line) else { return Line::Other };
    if let Some(err) = v.get("error") {
        return Line::Error(err.get_str("code").unwrap_or("error").to_string());
    }
    if v.get("event").is_some() {
        return Line::Event;
    }
    match v.get("result").and_then(|r| r.get_str("type")) {
        Some("subscription_started") => Line::Started,
        _ => Line::Other,
    }
}

/// Lifecycle subscriptions that can change which agents exist, where, or in which workspace.
const LIFECYCLE: &[&str] = &[
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.moved",
    "pane.updated",
    "pane.agent_detected",
    "workspace.created",
    "workspace.updated",
    "workspace.closed",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
];

/// The `events.subscribe` request: lifecycle events plus status changes of each agent pane.
pub fn subscribe_request(id: &str, panes: &BTreeSet<String>) -> String {
    let mut subs: Vec<String> = LIFECYCLE
        .iter()
        .map(|t| format!("{{\"type\":{}}}", json::quote(t)))
        .collect();
    subs.extend(panes.iter().map(|p| {
        format!(
            "{{\"type\":\"pane.agent_status_changed\",\"pane_id\":{}}}",
            json::quote(p)
        )
    }));
    format!(
        "{{\"id\":{},\"method\":\"events.subscribe\",\"params\":{{\"subscriptions\":[{}]}}}}",
        json::quote(id),
        subs.join(",")
    )
}

/// The socket to use: `HERDR_SOCKET_PATH`, else the `HERDR_SESSION` session's socket, else the
/// default session's, both under herdr's config dir (`$XDG_CONFIG_HOME/herdr`, else
/// `~/.config/herdr`; a node-local `XDG_CONFIG_HOME` is how a shared home keeps one herdr per
/// machine).
pub fn socket_path_from(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(p) = env("HERDR_SOCKET_PATH").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let config = match env("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(env("HOME").filter(|h| !h.is_empty())?).join(".config"),
    }
    .join("herdr");
    match env("HERDR_SESSION").filter(|s| !s.is_empty()) {
        Some(name) => Some(config.join("sessions").join(name).join("herdr.sock")),
        None => Some(config.join("herdr.sock")),
    }
}

pub fn socket_path() -> Option<PathBuf> {
    socket_path_from(|k| std::env::var(k).ok())
}

/// Where herdr creates worktrees: `[worktrees] directory` in its `config.toml`
/// (`$XDG_CONFIG_HOME/herdr`, else `~/.config/herdr`), default `~/.herdr/worktrees`.
pub fn worktree_dir_from(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let home = PathBuf::from(env("HOME").filter(|h| !h.is_empty())?);
    let config = env("XDG_CONFIG_HOME")
        .filter(|x| !x.is_empty())
        .map_or_else(|| home.join(".config"), PathBuf::from)
        .join("herdr/config.toml");
    let text = std::fs::read_to_string(config).unwrap_or_default();
    let dir = config_worktree_dir(&text).unwrap_or_else(|| "~/.herdr/worktrees".into());
    let dir = match dir.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(dir),
    };
    Some(std::fs::canonicalize(&dir).unwrap_or(dir))
}

pub fn worktree_dir() -> Option<PathBuf> {
    worktree_dir_from(|k| std::env::var(k).ok())
}

/// `directory` in the `[worktrees]` table of herdr's TOML config (only plain `key = "string"`
/// lines are understood, which is how herdr writes it).
fn config_worktree_dir(toml: &str) -> Option<String> {
    let mut table = "";
    for line in toml.lines().map(str::trim) {
        if let Some(t) = line.strip_prefix('[') {
            table = t.trim_end_matches(']').trim();
        } else if table == "worktrees"
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == "directory"
        {
            let v = v.trim();
            let v = v.split_once(" #").map_or(v, |(v, _)| v).trim();
            return Some(v.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
    }
    None
}

/// `pane.split` params for a focused shell in `cwd`, in a new pane below `pane_id` (canopy's own).
/// herdr starts the user's shell there; the pane closes when the shell exits.
pub fn shell_split_params(pane_id: &str, cwd: &Path) -> String {
    format!(
        "{{\"target_pane_id\":{},\"direction\":\"down\",\"cwd\":{},\"focus\":true}}",
        json::quote(pane_id),
        json::quote(&cwd.to_string_lossy())
    )
}

/// `pane.send_input` params that run `command` in a fresh shell pane: typed after a space (kept
/// out of shell history) as `exec`, so the pane closes when the command exits.
pub fn run_params(pane_id: &str, command: &str) -> String {
    format!(
        "{{\"pane_id\":{},\"text\":{},\"keys\":[\"Enter\"]}}",
        json::quote(pane_id),
        json::quote(&format!(" exec {command}"))
    )
}

#[cfg(unix)]
mod client;
#[cfg(unix)]
pub use client::{focus, focus_at, open_shell_pane, open_shell_pane_at, read_view_at, remove_worktree_at, spawn, spawn_with};

/// Without Unix sockets the feature is unavailable.
#[cfg(not(unix))]
pub fn spawn(_emit: impl Fn(Option<std::sync::Arc<View>>) + Send + 'static) {}
#[cfg(not(unix))]
pub fn focus(_pane_id: String) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_dir_config() {
        let toml = "[ui]\ndirectory = \"/nope\"\n\n[worktrees]\n# directory = \"~/old\"\ndirectory = \"~/trees\" # mine\n";
        assert_eq!(config_worktree_dir(toml).as_deref(), Some("~/trees"));
        assert_eq!(config_worktree_dir("[worktrees]\n"), None);
        let env = |k: &str| match k {
            "HOME" => Some("/nonexistent-home".to_string()),
            _ => None,
        };
        assert_eq!(worktree_dir_from(env), Some(PathBuf::from("/nonexistent-home/.herdr/worktrees")));
    }

    // Captured from herdr 0.9.3 (`agent.list` / `workspace.list`), trimmed and anonymized.
    const AGENT_LIST: &str = r#"{"id":"r1","result":{"type":"agent_list","agents":[
{"terminal_id":"t1","agent":"claude","agent_status":"working","cwd":"/w/repo","foreground_cwd":"/w/wt/feat/src","focused":false,"name":"feat","pane_id":"w1B:p1","revision":3,"tab_id":"w1B:t1","workspace_id":"w1B","state_labels":{},"tokens":{}},
{"terminal_id":"t2","agent":"claude","agent_status":"blocked","cwd":"/w/repo","foreground_cwd":null,"focused":true,"name":null,"pane_id":"w1:p1","revision":9,"tab_id":"w1:t1","workspace_id":"w1"},
{"terminal_id":"t3","agent":"codex","agent_status":"done","cwd":null,"foreground_cwd":null,"focused":false,"name":null,"pane_id":"w2:p1","revision":1,"tab_id":"w2:t1","workspace_id":"w2"},
{"terminal_id":"t4","agent":"claude","agent_status":"idle","cwd":"/elsewhere","foreground_cwd":"/elsewhere","focused":false,"name":"other","pane_id":"w3:p1","revision":1,"tab_id":"w3:t1","workspace_id":"w3"}]}}"#;
    const WORKSPACE_LIST: &str = r#"{"id":"r2","result":{"type":"workspace_list","workspaces":[
{"workspace_id":"w1","number":1,"label":"repo","focused":false,"pane_count":1,"tab_count":1,"active_tab_id":"w1:t1","agent_status":"blocked","worktree":{"repo_key":"/w/repo/.git","repo_name":"repo","repo_root":"/w/repo","checkout_path":"/w/repo","is_linked_worktree":false}},
{"workspace_id":"w2","number":2,"label":"fix","focused":false,"pane_count":1,"tab_count":1,"active_tab_id":"w2:t1","agent_status":"done","worktree":{"repo_key":"/w/repo/.git","repo_name":"repo","repo_root":"/w/repo","checkout_path":"/w/wt/fix","is_linked_worktree":true}},
{"workspace_id":"w3","number":3,"label":"misc","focused":false,"pane_count":1,"tab_count":1,"active_tab_id":"w3:t1","agent_status":"idle","worktree":null}]}}"#;

    fn view() -> View {
        View {
            agents: parse_agents(&Json::parse(AGENT_LIST).unwrap()).unwrap(),
            workspaces: parse_workspaces(&Json::parse(WORKSPACE_LIST).unwrap()).unwrap(),
        }
    }

    #[test]
    fn parses_responses() {
        let v = view();
        assert_eq!(v.agents.len(), 4);
        assert_eq!(v.agents[0].label, "feat");
        assert_eq!(v.agents[0].state, AgentState::Working);
        assert_eq!(
            v.agents[0].cwd.as_deref(),
            Some(Path::new("/w/wt/feat/src")),
            "foreground_cwd wins"
        );
        assert_eq!(v.agents[1].label, "claude", "no name: falls back to the kind");
        assert_eq!(
            v.agents[1].cwd.as_deref(),
            Some(Path::new("/w/repo")),
            "null foreground_cwd: cwd"
        );
        assert_eq!(v.agents[2].cwd, None);
        assert_eq!(v.workspaces[1].checkout.as_deref(), Some(Path::new("/w/wt/fix")));
        assert_eq!(v.workspaces[2].checkout, None);
        assert!(
            parse_agents(&Json::parse(WORKSPACE_LIST).unwrap()).is_none(),
            "wrong result type"
        );
    }

    #[test]
    fn maps_agents_to_worktrees() {
        let v = view();
        // Nested worktree under the main one: the deepest match wins.
        let wts = [
            PathBuf::from("/w/repo"),
            PathBuf::from("/w/wt/feat"),
            PathBuf::from("/w/wt/fix"),
            PathBuf::from("/w/repo/.claude/worktrees/x"),
        ];
        let a = assign(&v, &wts);
        assert_eq!(a[0], [1], "cwd /w/repo");
        assert_eq!(a[1], [0], "foreground_cwd inside the worktree");
        assert_eq!(a[2], [2], "no cwd: workspace checkout_path");
        assert!(a[3].is_empty());
        let nested = assign(
            &View {
                agents: vec![Agent {
                    cwd: Some("/w/repo/.claude/worktrees/x/src".into()),
                    ..v.agents[0].clone()
                }],
                workspaces: vec![],
            },
            &wts,
        );
        assert_eq!(nested[3], [0]);
        assert!(nested[0].is_empty());
        // `/w/repository` must not match `/w/repo` (component-wise prefix).
        let similar = assign(
            &View {
                agents: vec![Agent {
                    cwd: Some("/w/repository".into()),
                    ..v.agents[0].clone()
                }],
                workspaces: vec![],
            },
            &wts,
        );
        assert!(similar.iter().all(Vec::is_empty));
    }

    #[test]
    fn activity_is_most_urgent() {
        let mut v = view();
        v.agents[0].state = AgentState::Working;
        v.agents[2].state = AgentState::Blocked;
        assert_eq!(
            activity(&v, &[0, 2, 3]),
            Some(Activity {
                state: AgentState::Blocked,
                count: 3
            })
        );
        assert_eq!(activity(&v, &[]), None);
    }

    #[test]
    fn classifies_subscription_lines() {
        assert_eq!(
            classify(r#"{"id":"s1","result":{"type":"subscription_started"}}"#),
            Line::Started
        );
        assert_eq!(
            classify(
                r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w1:p1","workspace_id":"w1","agent_status":"blocked"}}"#
            ),
            Line::Event
        );
        assert_eq!(
            classify(r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p2"}}"#),
            Line::Event
        );
        assert_eq!(
            classify(r#"{"id":"s1","error":{"code":"events_lost","message":"subscriber fell behind"}}"#),
            Line::Error("events_lost".into())
        );
        assert_eq!(classify("not json"), Line::Other);
    }

    #[test]
    fn builds_subscribe_request() {
        let panes: BTreeSet<String> = ["w1:p1".to_string()].into();
        let req = Json::parse(&subscribe_request("s1", &panes)).unwrap();
        assert_eq!(req.get_str("method"), Some("events.subscribe"));
        let subs = req.get("params").unwrap().get("subscriptions").unwrap().arr().unwrap();
        assert_eq!(subs.len(), LIFECYCLE.len() + 1);
        let last = subs.last().unwrap();
        assert_eq!(last.get_str("type"), Some("pane.agent_status_changed"));
        assert_eq!(last.get_str("pane_id"), Some("w1:p1"));
    }

    #[test]
    fn resolves_socket_path() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(
            socket_path_from(env(&[
                ("HERDR_SOCKET_PATH", "/s.sock"),
                ("HERDR_SESSION", "x"),
                ("HOME", "/h")
            ])),
            Some(PathBuf::from("/s.sock"))
        );
        assert_eq!(
            socket_path_from(env(&[("HERDR_SESSION", "work"), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.config/herdr/sessions/work/herdr.sock"))
        );
        assert_eq!(
            socket_path_from(env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.config/herdr/herdr.sock"))
        );
        assert_eq!(
            socket_path_from(env(&[("XDG_CONFIG_HOME", "/tmp/me/xdg"), ("HOME", "/h")])),
            Some(PathBuf::from("/tmp/me/xdg/herdr/herdr.sock"))
        );
        assert_eq!(socket_path_from(env(&[])), None);
    }

    #[test]
    fn run_params_are_json() {
        let p = Json::parse(&run_params("w1:p3", "lazygit -p \"x\"")).unwrap();
        assert_eq!(p.get_str("pane_id"), Some("w1:p3"));
        assert_eq!(p.get_str("text"), Some(" exec lazygit -p \"x\""));
        assert_eq!(p.get("keys").and_then(Json::arr).map(<[Json]>::len), Some(1));
    }

    #[test]
    fn shell_split_params_are_json() {
        let p = Json::parse(&shell_split_params("w1:p2", Path::new("/w/a \"b\""))).unwrap();
        assert_eq!(p.get_str("target_pane_id"), Some("w1:p2"));
        assert_eq!(p.get_str("direction"), Some("down"));
        assert_eq!(p.get_str("cwd"), Some("/w/a \"b\""));
        assert_eq!(p.get("focus"), Some(&Json::Bool(true)));
    }
}
