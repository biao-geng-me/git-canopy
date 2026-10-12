//! Terminal UI. Runs on the main thread and only renders snapshots received over a channel;
//! all git work happens on the refresh worker (and on short-lived diff threads).
//!
//! Both overview panels are expandable lists (see [`inset`]): a row expands in place into its
//! diff summary, a file into its diff, and Enter opens the full-screen [`DiffView`].

pub mod copy;
mod diff_view;
mod inset;
mod render;
mod term;

use std::collections::HashMap;
use std::io::stdout;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Position, Rect};

use crate::cleanup;
use crate::diff::{self, DiffDoc};
use crate::git::RepoPaths;
use crate::graph::RowKind;
use crate::herdr;
use crate::model::{Cleanup, Snapshot};
use crate::watch::{self, Config, Request};
use diff_view::{Action, DiffView};
use inset::{Entry, Expansion, Item, Key, Open, VLine};

pub enum UiMsg {
    Input(Event),
    Snapshot(Arc<Snapshot>),
    Diff {
        id: u64,
        result: Result<DiffDoc, String>,
    },
    Inset {
        key: Key,
        req: u64,
        result: Result<DiffDoc, String>,
    },
    /// What herdr reports; `None` when herdr isn't reachable.
    Herdr(Option<Arc<herdr::View>>),
    /// The inset shell printed something (redraw).
    TermOutput,
    /// The inset shell with this id exited.
    TermExited(u64),
    /// Neither herdr nor tmux could split a pane for a shell (or this command) in this worktree:
    /// use the inset.
    ShellInset(PathBuf, Option<String>),
    /// A copy finished: the footer notice, and the OSC 52 sequence to write when no clipboard
    /// program or tmux took the text.
    Copied {
        notice: String,
        osc52: Option<String>,
    },
    /// The checks for removing a worktree finished: what removing it will do, for the confirm
    /// popup, or why it can't be removed.
    CleanupPlan(Result<Box<cleanup::Plan>, String>),
    /// Removing a worktree finished: the footer notice.
    CleanupDone(Result<String, String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Worktrees,
    Graph,
}

/// How the two overview panels are arranged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutMode {
    /// Graph on top, worktrees below, both full width (the default).
    #[default]
    Stacked,
    /// Worktrees on the left, graph on the right.
    Side,
}

/// Screen regions from the last render, for mouse hit-testing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Areas {
    pub worktrees: Rect,
    pub graph: Rect,
    /// The inset shell (empty when none is open).
    pub shell: Rect,
}

/// Where the clickable fields of a worktree row are (columns relative to the panel content).
#[derive(Clone, Debug, Default)]
pub struct WtFields {
    pub agent: Option<Range<u16>>,
    pub path: Option<Range<u16>>,
    pub overlap: Option<Range<u16>>,
}

/// Progress of a worktree removal started with Backspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Cleaning {
    #[default]
    Idle,
    /// Checking the worktree at `path` before the confirm popup. `confirmed`: Backspace was
    /// pressed again meanwhile, so it is removed as soon as the checks pass, without the popup.
    Checking { path: PathBuf, confirmed: bool },
    /// The confirm popup is open (`App::confirm`).
    Confirming,
    Removing,
}

/// A shell (or a git TUI) in an inset below the graph.
pub struct Shell {
    id: u64,
    /// What runs there: `shell`, or the command's program name.
    pub label: String,
    pub term: term::Term,
}

/// A vertically scrolling selection over a list of lines.
#[derive(Clone, Copy, Debug, Default)]
pub struct ListPos {
    pub sel: usize,
    pub offset: usize,
    /// Scroll the selection into view on the next render (cleared when the user scrolls the
    /// viewport with the mouse wheel instead).
    pub follow: bool,
}

impl ListPos {
    pub fn clamp(&mut self, len: usize) {
        self.sel = self.sel.min(len.saturating_sub(1));
        self.offset = self.offset.min(len.saturating_sub(1));
    }

    pub fn move_by(&mut self, delta: isize, len: usize) {
        let max = len.saturating_sub(1) as isize;
        self.sel = (self.sel as isize).saturating_add(delta).clamp(0, max.max(0)) as usize;
        self.follow = true;
    }

    pub fn scroll_by(&mut self, delta: isize, len: usize, height: usize) {
        let max = len.saturating_sub(height) as isize;
        self.offset = (self.offset as isize + delta).clamp(0, max.max(0)) as usize;
        self.follow = false;
    }

    /// Adjust the offset so the selection is visible in `height` rows.
    pub fn scroll_into_view(&mut self, height: usize) {
        if height == 0 || !self.follow {
            return;
        }
        if self.sel < self.offset {
            self.offset = self.sel;
        } else if self.sel >= self.offset + height {
            self.offset = self.sel + 1 - height;
        }
    }
}

/// One overview panel: its visual lines, cursor and expansion state.
#[derive(Default)]
pub struct Panel {
    pub pos: ListPos,
    pub exp: Expansion,
    pub lines: Vec<VLine>,
    /// Wrapped subject and message text of open rows, laid out for the panel width.
    pub texts: HashMap<usize, InsetText>,
}

/// Wrapped text of an open graph row.
#[derive(Clone, Debug, Default)]
pub struct InsetText {
    /// The subject: the first line goes on the row, the rest on continuation lines.
    pub subject: Vec<String>,
    /// The message body wrapped to the inset width, once loaded.
    pub message: Option<Vec<String>>,
}

/// Identity of a visual line, stable across refreshes (for keeping the cursor in place).
#[derive(Clone, Debug, PartialEq, Eq)]
struct LineId {
    row: Key,
    item: Option<ItemId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ItemId {
    Subject(usize),
    Message(usize),
    File(String),
    Diff(String, usize),
    More(String),
    Other,
}

struct Click {
    at: Instant,
    panel: Focus,
    line: usize,
    /// The line the click expanded/collapsed, if any (undone if it becomes a double-click).
    toggled: Option<(Focus, usize)>,
}

pub struct App {
    pub snap: Arc<Snapshot>,
    pub interval_ms: u128,
    pub layout: LayoutMode,
    pub focus: Focus,
    pub wt: Panel,
    pub graph: Panel,
    /// Diffs for insets, by target.
    pub docs: HashMap<Key, Entry>,
    /// herdr agent activity (`None`: herdr not reachable or disabled).
    pub herdr: Option<Arc<herdr::View>>,
    /// herdr agents per worktree (indexes into `herdr.agents`), parallel to the snapshot's worktrees.
    pub agents: Vec<Vec<usize>>,
    /// Pending "show this worktree in the graph" request, applied at render time (needs height).
    pub graph_jump: Option<PathBuf>,
    pub diff: Option<DiffView>,
    pub help: bool,
    pub quit: bool,
    pub areas: Areas,
    /// Graph width the open rows' text was wrapped for.
    pub wrap_width: usize,
    /// Clickable fields of the visible worktree rows, by worktree index (from the last render).
    pub wt_fields: HashMap<usize, WtFields>,
    pub shell: Option<Shell>,
    /// Keys go to the inset shell (Ctrl-] leaves it).
    pub shell_focus: bool,
    /// A one-off message for the footer, cleared by the next key or click.
    pub notice: Option<String>,
    /// `y` was pressed: the next key picks what to copy.
    pub copy_pending: bool,
    /// Removing a worktree, waiting for the user to confirm.
    pub confirm: Option<cleanup::Plan>,
    /// Where a worktree removal is (one at a time).
    cleaning: Cleaning,
    clipboard: Vec<crate::clipboard::Method>,
    /// herdr integration enabled (`--no-herdr` turns it off, the herdr shell pane included).
    with_herdr: bool,
    /// Where herdr creates worktrees: checkouts there are only cleaned up through a herdr
    /// workspace canopy can see.
    herdr_dir: Option<PathBuf>,
    /// Git TUI for `t` (`--git-tui` / `CANOPY_GIT_TUI`); found on PATH when `None`.
    git_tui: Option<String>,
    next_id: u64,
    tx: Sender<UiMsg>,
    last_click: Option<Click>,
    /// The mouse pointer's panel and screen row, for the hover highlight (cleared by a key).
    pub hover: Option<(Focus, u16)>,
    /// Whether the last handled message changed anything on screen.
    redraw: bool,
    /// Graph row count when "load more" was last requested, to ask only once per page.
    more_requested_at: usize,
    worker: watch::Handle,
}

impl App {
    fn new(worker: watch::Handle, tx: Sender<UiMsg>, cfg: &Config, layout: LayoutMode) -> Self {
        App {
            snap: Arc::new(Snapshot::default()),
            interval_ms: cfg.interval.as_millis(),
            layout,
            focus: Focus::Worktrees,
            wt: Panel::default(),
            graph: Panel::default(),
            docs: HashMap::new(),
            herdr: None,
            agents: Vec::new(),
            graph_jump: None,
            diff: None,
            help: false,
            quit: false,
            areas: Areas::default(),
            wrap_width: 0,
            wt_fields: HashMap::new(),
            shell: None,
            shell_focus: false,
            notice: None,
            copy_pending: false,
            confirm: None,
            cleaning: Cleaning::Idle,
            clipboard: crate::clipboard::methods(),
            with_herdr: true,
            herdr_dir: None,
            git_tui: None,
            next_id: 0,
            tx,
            last_click: None,
            hover: None,
            redraw: true,
            more_requested_at: 0,
            worker,
        }
    }

    pub fn panel(&self, f: Focus) -> &Panel {
        match f {
            Focus::Worktrees => &self.wt,
            Focus::Graph => &self.graph,
        }
    }

    fn panel_mut(&mut self, f: Focus) -> &mut Panel {
        match f {
            Focus::Worktrees => &mut self.wt,
            Focus::Graph => &mut self.graph,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Keys: what each base row is, and what it expands to

    /// Identity of base row `r` of a panel.
    pub fn row_key(&self, f: Focus, r: usize) -> Option<Key> {
        match f {
            Focus::Worktrees => self.snap.worktrees.get(r).map(|w| Key::Worktree(w.path.clone())),
            Focus::Graph => {
                let g = &self.snap.graph;
                Some(match g.rows.get(r)?.kind {
                    RowKind::Commit(ci) => Key::Commit(g.commits[ci].id.clone()),
                    RowKind::Uncommitted(wi) => Key::Worktree(self.snap.worktrees.get(wi)?.path.clone()),
                })
            }
        }
    }

    /// What base row `r` expands to / diffs: a commit, or a worktree's uncommitted changes.
    /// A clean worktree shows its HEAD commit.
    pub fn target(&self, f: Focus, r: usize) -> Option<Key> {
        match f {
            Focus::Graph => self.row_key(f, r),
            Focus::Worktrees => {
                let w = self.snap.worktrees.get(r)?;
                if !w.has_worktree() {
                    return None;
                }
                match (&w.status, &w.head) {
                    (Some(st), Some(h)) if !st.is_dirty() => Some(Key::Commit(h.clone())),
                    _ => Some(Key::Worktree(w.path.clone())),
                }
            }
        }
    }

    fn row_of(&self, f: Focus, key: &Key) -> Option<usize> {
        match (f, key) {
            (Focus::Worktrees, Key::Worktree(p)) => self.snap.worktrees.iter().position(|w| &w.path == p),
            (Focus::Worktrees, Key::Commit(_)) => None,
            (Focus::Graph, Key::Commit(id)) => self.snap.graph.commit_row.get(id).copied(),
            (Focus::Graph, Key::Worktree(p)) => {
                let g = &self.snap.graph;
                g.worktree_row
                    .get(p)
                    .copied()
                    .filter(|&r| matches!(g.rows[r].kind, RowKind::Uncommitted(_)))
            }
        }
    }

    /// The doc for an inset item's row.
    pub fn row_doc(&self, f: Focus, r: usize) -> Option<&Entry> {
        self.docs.get(&self.target(f, r)?)
    }

    fn file_path(&self, f: Focus, r: usize, fi: usize) -> Option<String> {
        Some(self.row_doc(f, r)?.ok()?.files.get(fi)?.path.clone())
    }

    // -----------------------------------------------------------------------------------------
    // Visual lines

    /// Rebuild both panels' visual lines, requesting docs for newly opened rows.
    /// Only the graph expands; the worktree panel is a plain list.
    fn rebuild(&mut self) {
        let wt = &mut self.wt;
        wt.lines = (0..self.snap.worktrees.len()).map(VLine::Row).collect();
        wt.pos.clamp(wt.lines.len());

        let f = Focus::Graph;
        let mut texts = HashMap::new();
        let opened: Vec<Open> = self
            .graph
            .exp
            .keys()
            .filter_map(|k| {
                let row = self.row_of(f, k)?;
                let text = render::inset_text(self, row, self.wrap_width);
                let open = Open {
                    row,
                    row_key: k.clone(),
                    target: self.target(f, row)?,
                    subject_lines: text.as_ref().map_or(1, |t| t.subject.len().max(1)),
                    message_lines: text.as_ref().and_then(|t| t.message.as_ref().map(Vec::len)),
                };
                if let Some(t) = text {
                    texts.insert(row, t);
                }
                Some(open)
            })
            .collect();
        for o in &opened {
            self.request_doc(&o.target);
        }
        let lines = inset::build(self.snap.graph.rows.len(), &opened, &self.graph.exp, &self.docs);
        let p = &mut self.graph;
        p.lines = lines;
        p.texts = texts;
        p.pos.clamp(p.lines.len());
    }

    /// Re-wrap open rows for a new graph width, keeping the cursor on the same line.
    pub(super) fn set_wrap_width(&mut self, width: usize) {
        if width != self.wrap_width {
            self.wrap_width = width;
            let id = self.line_id(Focus::Graph);
            self.rebuild();
            self.relocate(Focus::Graph, id);
        }
    }

    /// Esc: fold the expansion around the cursor (back to its row), or every expansion.
    fn fold_row(&mut self, f: Focus) {
        let sel = self.panel(f).pos.sel;
        let Some(&line) = self.panel(f).lines.get(sel) else {
            return;
        };
        let row = line.row();
        match self.row_key(f, row) {
            Some(k) if self.panel(f).exp.is_open(&k) => {
                let header = inset::index_of_row(&self.panel(f).lines, row);
                self.panel_mut(f).exp.close(&k);
                if let Some(h) = header {
                    self.set_cursor(f, h);
                }
            }
            _ => {
                let id = self.line_id(f);
                self.panel_mut(f).exp = Expansion::default();
                self.rebuild();
                self.relocate(f, id);
                self.panel_mut(f).pos.follow = true;
            }
        }
    }

    fn line_id(&self, f: Focus) -> Option<LineId> {
        let p = self.panel(f);
        let line = *p.lines.get(p.pos.sel)?;
        let row = self.row_key(f, line.row())?;
        let item = match line {
            VLine::Row(_) => None,
            VLine::Inset(r, item) => Some(match item {
                Item::Subject(i) => ItemId::Subject(i),
                Item::Message(i) => ItemId::Message(i),
                Item::File(fi) => ItemId::File(self.file_path(f, r, fi)?),
                Item::Diff(fi, li) => ItemId::Diff(self.file_path(f, r, fi)?, li),
                Item::More(fi, _) => ItemId::More(self.file_path(f, r, fi)?),
                Item::Loading | Item::Error | Item::Empty => ItemId::Other,
            }),
        };
        Some(LineId { row, item })
    }

    /// Put the cursor back on the line with this identity (or its row) after a rebuild,
    /// shifting the viewport along so the view doesn't jump.
    fn relocate(&mut self, f: Focus, id: Option<LineId>) {
        let Some(id) = id else { return };
        let Some(r) = self.row_of(f, &id.row).or_else(|| match (&id.row, f) {
            // A worktree that became clean: its uncommitted row is gone, use its HEAD.
            (Key::Worktree(p), Focus::Graph) => self.snap.graph.worktree_row.get(p).copied(),
            _ => None,
        }) else {
            return;
        };
        let p = self.panel(f);
        let Some(row_idx) = inset::index_of_row(&p.lines, r) else {
            return;
        };
        let same_row = |i: &usize| p.lines.get(*i).is_some_and(|l| l.row() == r);
        let found = match &id.item {
            None | Some(ItemId::Other) => Some(row_idx),
            Some(item) => (row_idx..p.lines.len())
                .take_while(same_row)
                .find(|&i| {
                    let VLine::Inset(_, it) = p.lines[i] else { return false };
                    match (item, it) {
                        (ItemId::Subject(a), Item::Subject(b)) => *a == b,
                        (ItemId::Message(a), Item::Message(b)) => *a == b,
                        (ItemId::File(path), Item::File(fi)) => self.file_path(f, r, fi).as_ref() == Some(path),
                        (ItemId::Diff(path, a), Item::Diff(fi, b)) => {
                            *a == b && self.file_path(f, r, fi).as_ref() == Some(path)
                        }
                        (ItemId::More(path), Item::More(fi, _)) => self.file_path(f, r, fi).as_ref() == Some(path),
                        _ => false,
                    }
                })
                .or(Some(row_idx)),
        };
        if let Some(idx) = found {
            let p = self.panel_mut(f);
            let shift = idx as isize - p.pos.sel as isize;
            p.pos.offset = (p.pos.offset as isize + shift).max(0) as usize;
            p.pos.sel = idx;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Docs (diffs) for insets

    fn worktree_stamp(&self, key: &Key) -> Option<(Option<crate::git::status::Status>, Option<i64>)> {
        let Key::Worktree(p) = key else { return None };
        let w = self.snap.worktrees.iter().find(|w| &w.path == p)?;
        Some((w.status.clone(), w.changed_at))
    }

    /// For an open overlaps view: the state of the other worktrees in it, which it shows too.
    fn overlap_side_stamps(&self) -> Vec<Option<(Option<crate::git::status::Status>, Option<i64>)>> {
        match self.diff.as_ref().map(|d| &d.target) {
            Some(diff::Target::Overlaps { overlaps, .. }) => overlaps
                .iter()
                .filter_map(|o| o.path.clone())
                .map(|p| self.worktree_stamp(&Key::Worktree(p)))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn diff_target(&self, key: &Key) -> Option<diff::Target> {
        Some(match key {
            Key::Commit(id) => diff::Target::Commit {
                dir: self.snap.common_dir.clone(),
                id: id.clone(),
            },
            Key::Worktree(p) => {
                let w = self.snap.worktrees.iter().find(|w| &w.path == p)?;
                diff::Target::Worktree {
                    path: p.clone(),
                    has_head: w.head.is_some(),
                }
            }
        })
    }

    /// Start loading `key` unless it is loaded (and fresh) or already loading.
    fn request_doc(&mut self, key: &Key) {
        if self.docs.contains_key(key) {
            return;
        }
        let Some(target) = self.diff_target(key) else { return };
        self.next_id += 1;
        let req = self.next_id;
        self.docs.insert(
            key.clone(),
            Entry {
                req,
                stamp: self.worktree_stamp(key),
                ..Default::default()
            },
        );
        let (tx, key) = (self.tx.clone(), key.clone());
        spawn_named("canopy-inset", move || {
            let result = diff::load(&target).map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiMsg::Inset { key, req, result });
        });
    }

    /// Worktree docs whose status changed are reloaded if shown, dropped otherwise. Commit docs
    /// never go stale; the cache is trimmed to what is open when it grows.
    fn refresh_docs(&mut self) {
        let stale: Vec<Key> = self
            .docs
            .iter()
            .filter(|(k, e)| matches!(k, Key::Worktree(_)) && e.doc.is_some() && self.worktree_stamp(k) != e.stamp)
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            let shown = self.wt.exp.is_open(&key) || self.graph.exp.is_open(&key);
            if !shown {
                self.docs.remove(&key);
                continue;
            }
            // Keep showing the old doc until the new one arrives.
            let Some(target) = self.diff_target(&key) else { continue };
            self.next_id += 1;
            let req = self.next_id;
            let stamp = self.worktree_stamp(&key);
            if let Some(e) = self.docs.get_mut(&key) {
                e.req = req;
                e.stamp = stamp;
            }
            let tx = self.tx.clone();
            spawn_named("canopy-inset", move || {
                let result = diff::load(&target).map_err(|e| format!("{e:#}"));
                let _ = tx.send(UiMsg::Inset { key, req, result });
            });
        }
        if self.docs.len() > 256 {
            let open: Vec<Key> = self.wt.exp.keys().chain(self.graph.exp.keys()).cloned().collect();
            self.docs.retain(|k, _| open.contains(k));
        }
    }

    // -----------------------------------------------------------------------------------------
    // Messages

    fn handle(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Snapshot(s) => {
                let ids = (self.line_id(Focus::Worktrees), self.line_id(Focus::Graph));
                self.on_snapshot(s, ids);
            }
            UiMsg::Inset { key, req, result } => {
                let ids = (self.line_id(Focus::Worktrees), self.line_id(Focus::Graph));
                if let Some(e) = self.docs.get_mut(&key).filter(|e| e.req == req) {
                    e.set(result);
                }
                self.rebuild();
                self.relocate(Focus::Worktrees, ids.0);
                self.relocate(Focus::Graph, ids.1);
            }
            UiMsg::Herdr(view) => {
                self.herdr = view;
                self.map_agents();
            }
            UiMsg::TermOutput => {}
            UiMsg::TermExited(id) => {
                if self.shell.as_ref().is_some_and(|s| s.id == id) {
                    self.shell = None;
                    self.shell_focus = false;
                }
            }
            UiMsg::ShellInset(path, command) => self.open_shell_inset(&path, command),
            UiMsg::Copied { notice, osc52 } => {
                if let Some(seq) = osc52 {
                    // Between frames, so it can't land inside ratatui's output.
                    use std::io::Write as _;
                    let mut out = std::io::stdout();
                    let _ = out.write_all(seq.as_bytes()).and_then(|()| out.flush());
                }
                self.notice = Some(notice);
            }
            UiMsg::CleanupPlan(plan) => match plan {
                Ok(plan) if matches!(self.cleaning, Cleaning::Checking { confirmed: true, .. }) => {
                    self.remove_worktree(*plan);
                }
                Ok(plan) => {
                    self.notice = None;
                    self.confirm = Some(*plan);
                    self.cleaning = Cleaning::Confirming;
                }
                Err(e) => {
                    self.cleaning = Cleaning::Idle;
                    self.notice = Some(e);
                }
            },
            UiMsg::CleanupDone(result) => {
                self.cleaning = Cleaning::Idle;
                self.notice = Some(result.unwrap_or_else(|e| e));
                self.worker.request(Request::Refresh);
            }
            UiMsg::Diff { id, result } => {
                if let Some(d) = self.diff.as_mut().filter(|d| d.id == id) {
                    d.loaded(result);
                }
            }
            UiMsg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                self.hover = None;
                if let Some(d) = self.diff.as_mut() {
                    d.hover = None;
                }
                if self.confirm.is_some() {
                    self.on_confirm_key(k);
                } else if std::mem::take(&mut self.copy_pending) {
                    self.on_copy_key(k);
                } else if self.starts_copy(k) {
                    self.notice = None;
                    self.copy_pending = true;
                } else {
                    match self.diff.as_mut() {
                        Some(d) if !self.help => {
                            self.notice = None;
                            let action = d.on_key(k);
                            self.diff_action(action);
                        }
                        _ => self.on_key(k),
                    }
                }
                self.rebuild();
            }
            UiMsg::Input(Event::Mouse(m)) if m.kind == MouseEventKind::Moved => {
                let pos = Position::new(m.column, m.row);
                let blocked = self.confirm.is_some() || self.help;
                if let Some(d) = self.diff.as_mut() {
                    let hover = d.hover_at(pos).filter(|_| !blocked);
                    self.redraw = hover != d.hover;
                    d.hover = hover;
                } else {
                    let hover = self.hover_at(pos).filter(|_| !blocked);
                    self.redraw = hover != self.hover;
                    self.hover = hover;
                }
            }
            UiMsg::Input(Event::Mouse(_)) if self.confirm.is_some() => {}
            UiMsg::Input(Event::Mouse(m)) => {
                match self.diff.as_mut() {
                    Some(d) => {
                        let action = d.on_mouse(m);
                        self.diff_action(action);
                    }
                    None => self.on_mouse(m),
                }
                self.rebuild();
            }
            UiMsg::Input(_) => {}
        }
    }

    fn on_snapshot(&mut self, snap: Arc<Snapshot>, ids: (Option<LineId>, Option<LineId>)) {
        let first_wts = self.snap.worktrees.is_empty();
        let first_graph = self.snap.graph.rows.is_empty();
        let graph_changed = !Arc::ptr_eq(&self.snap.graph, &snap.graph);
        let others_before = self.overlap_side_stamps();
        self.snap = snap;
        let others_changed = others_before != self.overlap_side_stamps();
        self.map_agents();
        self.refresh_docs();
        self.rebuild();

        if first_wts {
            // Start on the worktree canopy was launched from.
            if let Some(i) = self.snap.worktrees.iter().position(|w| w.is_current)
                && let Some(idx) = inset::index_of_row(&self.wt.lines, i)
            {
                self.wt.pos.sel = idx;
                self.wt.pos.follow = true;
            }
        } else {
            self.relocate(Focus::Worktrees, ids.0);
        }
        if first_graph && !self.snap.graph.rows.is_empty() {
            self.graph_jump = self.selected_worktree().map(|w| w.path.clone());
        } else if graph_changed {
            self.relocate(Focus::Graph, ids.1);
        }

        // Keep an open worktree diff in sync when that worktree changes.
        let stale = match &self.diff {
            Some(d @ DiffView { doc: Some(_), .. }) => match d.target.worktree() {
                Some(path) => {
                    let now = self.worktree_stamp(&Key::Worktree(path.to_path_buf()));
                    now.is_some_and(|now| now.0.is_some() && Some(now) != d.stamp)
                }
                None => false,
            },
            _ => false,
        };
        // An overlaps view follows the worktree's overlaps (other worktrees changing).
        let overlaps_now = match self.diff.as_ref().map(|d| &d.target) {
            Some(diff::Target::Overlaps { path, overlaps, .. }) => self
                .snap
                .worktrees
                .iter()
                .find(|w| &w.path == path)
                .map(|w| &w.overlaps)
                .filter(|now| *now != overlaps)
                .cloned(),
            _ => None,
        };
        let overlaps_changed = overlaps_now.is_some();
        if let Some(now) = overlaps_now
            && let Some(diff::Target::Overlaps { overlaps, .. }) = self.diff.as_mut().map(|d| &mut d.target)
        {
            *overlaps = now;
        }
        let loaded = self.diff.as_ref().is_some_and(|d| d.doc.is_some());
        if stale || (loaded && (overlaps_changed || others_changed)) {
            self.reload_diff();
        }
    }

    /// Match herdr agents to worktrees (after either side changed).
    fn map_agents(&mut self) {
        self.agents = match &self.herdr {
            Some(view) => {
                let paths: Vec<PathBuf> = self.snap.worktrees.iter().map(|w| w.path.clone()).collect();
                herdr::assign(view, &paths)
            }
            None => Vec::new(),
        };
    }

    /// The agents working in worktree `wi`, summarized.
    pub fn agent_activity(&self, wi: usize) -> Option<herdr::Activity> {
        herdr::activity(self.herdr.as_ref()?, self.agents.get(wi)?)
    }

    /// Whether worktree `wi` could be removed (with its branch) without losing anything. Any
    /// agent in it that isn't idle or done counts as busy.
    pub fn cleanup(&self, wi: usize) -> Cleanup {
        let Some(w) = self.snap.worktrees.get(wi) else { return Cleanup::No };
        if self.herdr_unseen(wi) {
            return Cleanup::No;
        }
        w.cleanup(self.snap.base.as_deref(), self.agent_busy(wi))
    }

    /// Why worktree `wi` can't be cleaned up (untracked files aside).
    fn cleanup_blocker(&self, wi: usize) -> Option<String> {
        let w = self.snap.worktrees.get(wi)?;
        w.cleanup_blocker(self.snap.base.as_deref(), self.agent_busy(wi))
            .or_else(|| self.herdr_unseen(wi).then(|| crate::model::HERDR_UNSEEN.to_string()))
    }

    /// Whether herdr made worktree `wi` but no herdr workspace canopy can see has it, so agents
    /// in it (say, from a herdr on another machine sharing the filesystem) would go unnoticed.
    fn herdr_unseen(&self, wi: usize) -> bool {
        let Some(w) = self.snap.worktrees.get(wi) else { return false };
        self.herdr_dir.as_deref().is_some_and(|d| w.path.starts_with(d))
            && !self
                .herdr
                .as_ref()
                .is_some_and(|v| v.workspaces.iter().any(|ws| ws.checkout.as_deref() == Some(&w.path)))
    }

    fn agent_busy(&self, wi: usize) -> bool {
        self.agent_activity(wi)
            .is_some_and(|a| !matches!(a.state, herdr::AgentState::Idle | herdr::AgentState::Done))
    }

    /// How many of this repo's agents are in `state`.
    pub fn agents_in_state(&self, state: herdr::AgentState) -> usize {
        let Some(view) = &self.herdr else { return 0 };
        self.agents
            .iter()
            .flatten()
            .filter(|&&i| view.agents[i].state == state)
            .count()
    }

    /// The worktree the cursor is on: the selected one in the worktree list; in the graph, the
    /// one the row belongs to.
    fn focused_wi(&self) -> Option<usize> {
        match self.focus {
            Focus::Worktrees => self.wt.lines.get(self.wt.pos.sel).map(|l| l.row()),
            Focus::Graph => self.graph_row_worktree(),
        }
    }

    fn focused_worktree(&self) -> Option<&crate::model::WorktreeView> {
        self.snap.worktrees.get(self.focused_wi()?).filter(|w| w.has_worktree())
    }

    /// `a`: ask herdr to focus the pane of the selected worktree's most urgent agent.
    fn focus_agent(&mut self) {
        let Some(view) = &self.herdr else { return };
        let wi = self.focused_wi();
        let agent = wi
            .and_then(|wi| self.agents.get(wi))
            .and_then(|ids| ids.iter().map(|&i| &view.agents[i]).max_by_key(|a| a.state));
        if let Some(a) = agent {
            herdr::focus(a.pane_id.clone());
        }
    }

    /// The worktree a graph row belongs to: its uncommitted row, or a commit labelled with it.
    fn graph_row_worktree(&self) -> Option<usize> {
        let r = self.graph.lines.get(self.graph.pos.sel)?.row();
        let g = &self.snap.graph;
        match g.rows.get(r)?.kind {
            RowKind::Uncommitted(wi) => Some(wi),
            RowKind::Commit(ci) => {
                let id = &g.commits[ci].id;
                let here = |wi: &usize| self.snap.worktrees[*wi].head.as_ref() == Some(id);
                let mut candidates = (0..self.snap.worktrees.len()).filter(here);
                let first = candidates.clone().next();
                candidates.find(|&wi| self.agent_activity(wi).is_some()).or(first)
            }
        }
    }

    /// The worktree under the worktree panel's cursor.
    pub fn selected_worktree(&self) -> Option<&crate::model::WorktreeView> {
        let line = self.wt.lines.get(self.wt.pos.sel)?;
        self.snap.worktrees.get(line.row())
    }

    // -----------------------------------------------------------------------------------------
    // Keyboard and mouse

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        self.notice = None;
        if self.shell_focus
            && let Some(shell) = self.shell.as_mut()
        {
            // Everything goes to the shell except Ctrl-], which hands the keys back to canopy.
            match k.code {
                KeyCode::Char(']' | '5') if ctrl => self.shell_focus = false,
                _ => shell.term.key(k),
            }
            return;
        }
        if self.help {
            // Any key closes the help popup; Ctrl-C still quits.
            self.help = false;
            if !(ctrl && k.code == KeyCode::Char('c')) {
                return;
            }
        }
        let f = self.focus;
        match k.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => self.worker.request(Request::Refresh),
            KeyCode::Char('a') => self.focus_agent(),
            KeyCode::Char('s') => {
                if let Some(w) = self.focused_worktree() {
                    self.open_shell(w.path.clone(), None);
                }
            }
            KeyCode::Char('t') => self.open_git_tui(),
            KeyCode::Backspace => self.start_cleanup(),
            KeyCode::Char('b') => self.open_branch_diff(),
            KeyCode::Char('o') => {
                if let Some(wi) = self.focused_wi() {
                    self.open_overlaps(wi);
                }
            }
            KeyCode::Char('v') => {
                self.layout = match self.layout {
                    LayoutMode::Stacked => LayoutMode::Side,
                    LayoutMode::Side => LayoutMode::Stacked,
                };
                self.panel_mut(Focus::Graph).pos.follow = true;
            }
            KeyCode::Enter => self.open_full(f, self.panel(f).pos.sel),
            // From the worktree list, expanding acts on that worktree's row in the graph.
            KeyCode::Char(' ')
            | KeyCode::Esc
            | KeyCode::Right
            | KeyCode::Char('l')
            | KeyCode::Left
            | KeyCode::Char('h')
                if f == Focus::Worktrees =>
            {
                if self.graph_cursor_to_selected_worktree() {
                    let g = Focus::Graph;
                    match k.code {
                        KeyCode::Char(' ') => self.space(g),
                        KeyCode::Esc => self.fold_row(g),
                        KeyCode::Right | KeyCode::Char('l') => self.expand(g),
                        _ => self.collapse(g),
                    }
                }
            }
            KeyCode::Char(' ') => self.space(f),
            KeyCode::Esc => self.fold_row(f),
            KeyCode::Right | KeyCode::Char('l') => self.expand(f),
            KeyCode::Left | KeyCode::Char('h') => self.collapse(f),
            // Tab: worktrees → graph → shell (when open) → worktrees.
            KeyCode::Tab | KeyCode::BackTab => match self.focus {
                Focus::Worktrees => self.focus = Focus::Graph,
                Focus::Graph if self.shell.is_some() => {
                    self.focus = Focus::Worktrees;
                    self.shell_focus = true;
                }
                Focus::Graph => self.focus = Focus::Worktrees,
            },
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Char('d') if ctrl => self.move_sel(self.page() as isize / 2),
            KeyCode::Char('u') if ctrl => self.move_sel(-(self.page() as isize) / 2),
            KeyCode::PageDown => self.move_sel(self.page() as isize),
            KeyCode::PageUp => self.move_sel(-(self.page() as isize)),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX),
            _ => {}
        }
    }

    fn page(&self) -> usize {
        let area = match self.focus {
            Focus::Worktrees => self.areas.worktrees,
            Focus::Graph => self.areas.graph,
        };
        (render::content(area).height as usize).max(1)
    }

    fn move_sel(&mut self, delta: isize) {
        let f = self.focus;
        let before = self.panel(f).lines.get(self.panel(f).pos.sel).map(|l| l.row());
        let p = self.panel_mut(f);
        p.pos.move_by(delta, p.lines.len());
        let after = p.lines.get(p.pos.sel).map(|l| l.row());
        match f {
            Focus::Worktrees if before != after => self.jump_graph_to_selected(),
            Focus::Graph => self.maybe_load_more(),
            _ => {}
        }
    }

    /// Put the graph cursor on the selected worktree's row (its uncommitted row or HEAD), so
    /// expand/collapse keys pressed in the worktree list act there. Focus stays on the list.
    /// Returns false if the worktree has no row in the graph.
    fn graph_cursor_to_selected_worktree(&mut self) -> bool {
        let Some(path) = self.selected_worktree().map(|w| w.path.clone()) else {
            return false;
        };
        let Some(&r) = self.snap.graph.worktree_row.get(&path) else {
            return false;
        };
        let Some(idx) = inset::index_of_row(&self.graph.lines, r) else {
            return false;
        };
        self.set_cursor(Focus::Graph, idx);
        true
    }

    fn jump_graph_to_selected(&mut self) {
        self.graph_jump = self.selected_worktree().map(|w| w.path.clone());
    }

    /// Ask the worker for more history when the cursor or view nears the end of what is loaded.
    fn maybe_load_more(&mut self) {
        let rows = self.snap.graph.rows.len();
        let (len, page) = (self.graph.lines.len(), self.page());
        let pos = self.graph.pos;
        let near_end = pos.sel + 2 * page >= len || pos.offset + 2 * page >= len;
        if self.snap.graph.truncated && near_end && self.more_requested_at != rows {
            self.more_requested_at = rows;
            self.worker.request(Request::LoadMore);
        }
    }

    /// Expand or collapse what is at `line`: a row's summary or a file's diff. Returns whether
    /// anything changed.
    fn toggle_at(&mut self, f: Focus, line: usize) -> bool {
        if f == Focus::Worktrees {
            return false;
        }
        let Some(&l) = self.panel(f).lines.get(line) else {
            return false;
        };
        let Some(rk) = self.row_key(f, l.row()) else {
            return false;
        };
        match l {
            VLine::Row(r) => {
                if self.target(f, r).is_none() {
                    return false;
                }
                self.panel_mut(f).exp.toggle(&rk);
                true
            }
            VLine::Inset(r, Item::File(fi)) => {
                let Some(path) = self.file_path(f, r, fi) else {
                    return false;
                };
                self.panel_mut(f).exp.toggle_file(&rk, &path);
                true
            }
            _ => false,
        }
    }

    /// Space: toggle a row or file; inside a diff or the message, fold the enclosing section.
    fn space(&mut self, f: Focus) {
        if f == Focus::Worktrees {
            return;
        }
        let sel = self.panel(f).pos.sel;
        match self.panel(f).lines.get(sel) {
            Some(VLine::Row(_)) | Some(VLine::Inset(_, Item::File(_))) => {
                self.toggle_at(f, sel);
            }
            Some(_) => self.collapse(f),
            None => {}
        }
    }

    fn expand(&mut self, f: Focus) {
        if f == Focus::Worktrees {
            return;
        }
        let sel = self.panel(f).pos.sel;
        let Some(&l) = self.panel(f).lines.get(sel) else { return };
        let Some(rk) = self.row_key(f, l.row()) else { return };
        match l {
            VLine::Row(r) if self.target(f, r).is_some() => self.panel_mut(f).exp.open(&rk),
            VLine::Inset(r, Item::File(fi)) => {
                if let Some(path) = self.file_path(f, r, fi)
                    && !self.panel(f).exp.file_open(&rk, &path)
                {
                    self.panel_mut(f).exp.toggle_file(&rk, &path);
                }
            }
            _ => {}
        }
    }

    /// Fold the innermost open section around the cursor and move the cursor to its header.
    fn collapse(&mut self, f: Focus) {
        let sel = self.panel(f).pos.sel;
        let Some(&l) = self.panel(f).lines.get(sel) else { return };
        let Some(rk) = self.row_key(f, l.row()) else { return };
        let lines = &self.panel(f).lines;
        match l {
            VLine::Row(_) => self.panel_mut(f).exp.close(&rk),
            VLine::Inset(r, Item::Diff(fi, _) | Item::More(fi, _)) => {
                let header = (0..sel).rev().find(|&i| lines[i] == VLine::Inset(r, Item::File(fi)));
                if let Some(path) = self.file_path(f, r, fi) {
                    self.panel_mut(f).exp.close_file(&rk, &path);
                }
                if let Some(h) = header {
                    self.set_cursor(f, h);
                }
            }
            VLine::Inset(r, _) => {
                let header = inset::index_of_row(lines, r);
                self.panel_mut(f).exp.close(&rk);
                if let Some(h) = header {
                    self.set_cursor(f, h);
                }
            }
        }
    }

    fn set_cursor(&mut self, f: Focus, idx: usize) {
        let p = self.panel_mut(f);
        p.pos.sel = idx;
        p.pos.follow = true;
    }

    /// Which field of a worktree row is at column `col` (of the panel content).
    fn wt_field_at(&self, f: Focus, line: usize, col: u16) -> Option<WtField> {
        if f != Focus::Worktrees {
            return None;
        }
        let wi = self.wt.lines.get(line)?.row();
        let fields = self.wt_fields.get(&wi)?;
        if fields.agent.as_ref().is_some_and(|r| r.contains(&col)) {
            return Some(WtField::Agent);
        }
        if fields.path.as_ref().is_some_and(|r| r.contains(&col)) {
            return Some(WtField::Path(self.snap.worktrees.get(wi)?.path.clone()));
        }
        if fields.overlap.as_ref().is_some_and(|r| r.contains(&col)) {
            return Some(WtField::Overlap(wi));
        }
        None
    }

    /// A shell in worktree `path` (running `command` if given): a pane split below canopy's if
    /// canopy runs in herdr or tmux, else an inset below the graph. The splits are requested off
    /// the UI thread.
    fn open_shell(&mut self, path: PathBuf, command: Option<String>) {
        if let Some(shell) = &self.shell {
            // One inset at a time: focus the open one rather than hang it up.
            self.shell_focus = true;
            if shell.term.path != path || command.is_some() {
                self.notice = Some(format!(
                    "{} is open in {}; exit it first",
                    shell.label,
                    render::tilde(&shell.term.path)
                ));
            }
            return;
        }
        let (tx, with_herdr) = (self.tx.clone(), self.with_herdr);
        spawn_named("canopy-shell", move || {
            let cmd = command.as_deref();
            if (with_herdr && herdr::open_shell_pane(&path, cmd).is_ok()) || tmux_split(&path, cmd) {
                return;
            }
            let _ = tx.send(UiMsg::ShellInset(path, command));
        });
    }

    /// Backspace: remove the focused worktree with its branch (and herdr workspace) once the user
    /// confirms. Only for worktrees marked `⌫` (their work is in base, nothing would be lost);
    /// everything is checked again off the UI thread before the confirm popup opens.
    fn start_cleanup(&mut self) {
        let target = self.cleanup_target();
        match &mut self.cleaning {
            Cleaning::Idle => {}
            // A double tap: remove it right after the checks, without the popup.
            Cleaning::Checking { path, confirmed } if target.map(|wi| &self.snap.worktrees[wi].path) == Some(path) => {
                *confirmed = true;
                return;
            }
            _ => {
                self.notice = Some("a removal is already in progress".into());
                return;
            }
        }
        let Some(wi) = target else { return };
        let w = self.snap.worktrees[wi].clone();
        match self.cleanup(wi) {
            Cleanup::Safe => {}
            Cleanup::Untracked => {
                let n = w.status.as_ref().map_or(0, |s| s.untracked);
                self.notice = Some(format!(
                    "can't remove {}: {n} untracked file{} would be lost; delete them first (s opens a shell there)",
                    w.name,
                    if n == 1 { "" } else { "s" }
                ));
                return;
            }
            Cleanup::No => {
                let why = self.cleanup_blocker(wi).unwrap_or_else(|| "it isn't safe to remove".into());
                self.notice = Some(format!("can't remove {}: {why}", w.name));
                return;
            }
        }
        let Some(base) = self.snap.base.clone() else { return };
        let common = self.snap.common_dir.clone();
        let sock = if self.with_herdr && self.herdr.is_some() { herdr::socket_path() } else { None };
        let herdr_dir = self.herdr_dir.clone();
        let tx = self.tx.clone();
        self.cleaning = Cleaning::Checking {
            path: w.path.clone(),
            confirmed: false,
        };
        self.notice = Some(format!("checking {}…", w.name));
        spawn_named("canopy-cleanup", move || {
            let own = std::env::var("HERDR_WORKSPACE_ID").ok();
            let plan = cleanup::plan(&common, &w, &base, sock, herdr_dir.as_deref(), own.as_deref())
                .map(Box::new)
                .map_err(|e| format!("can't remove {}: {e:#}", w.name));
            let _ = tx.send(UiMsg::CleanupPlan(plan));
        });
    }

    /// The worktree Backspace acts on. On a graph commit that several worktrees have checked out
    /// (a merged branch and its base), the one that can be cleaned up.
    fn cleanup_target(&self) -> Option<usize> {
        let focused = self.focused_wi()?;
        if self.focus != Focus::Graph {
            return Some(focused);
        }
        let head = self.snap.worktrees[focused].head.as_ref();
        let candidates: Vec<usize> = (0..self.snap.worktrees.len())
            .filter(|&wi| head.is_some() && self.snap.worktrees[wi].head.as_ref() == head)
            .collect();
        [Cleanup::Safe, Cleanup::Untracked]
            .iter()
            .find_map(|c| candidates.iter().copied().find(|&wi| self.cleanup(wi) == *c))
            .or(Some(focused))
    }

    /// Carry out a confirmed removal in the background (it checks everything once more).
    fn remove_worktree(&mut self, plan: cleanup::Plan) {
        self.cleaning = Cleaning::Removing;
        self.notice = Some(format!("removing {}…", plan.name));
        let tx = self.tx.clone();
        spawn_named("canopy-cleanup", move || {
            let own = std::env::var("HERDR_WORKSPACE_ID").ok();
            let res = cleanup::run(&plan, own.as_deref()).map_err(|e| format!("didn't remove {}: {e:#}", plan.name));
            let _ = tx.send(UiMsg::CleanupDone(res));
        });
    }

    /// Keys while the confirm popup is open: `y`/Enter/Backspace removes, Esc/`n`/`q` cancels.
    fn on_confirm_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('y') | KeyCode::Enter | KeyCode::Backspace => {
                if let Some(plan) = self.confirm.take() {
                    self.remove_worktree(plan);
                }
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                self.confirm = None;
                self.cleaning = Cleaning::Idle;
                self.notice = None;
            }
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            _ => {}
        }
    }

    /// `t`: a git TUI (lazygit, gitui, tig, or `--git-tui`) in the focused worktree, opened like
    /// a shell. canopy itself stays read-only; changes are made there.
    fn open_git_tui(&mut self) {
        let Some(path) = self.focused_worktree().map(|w| w.path.clone()) else {
            return;
        };
        match self.git_tui.clone().or_else(find_git_tui) {
            Some(cmd) => self.open_shell(path, Some(cmd)),
            None => {
                self.notice =
                    Some("no git TUI found (lazygit, gitui, tig); set one with --git-tui or CANOPY_GIT_TUI".into())
            }
        }
    }

    /// `b`: everything the focused worktree changed since it forked from the base branch.
    fn open_branch_diff(&mut self) {
        let Some(w) = self.focused_worktree() else { return };
        let Some(base) = self.snap.base.clone() else {
            self.notice = Some("no base branch to compare with (set one with --base)".into());
            return;
        };
        let (path, name) = (w.path.clone(), w.name.clone());
        let title = format!("[{name}] since branching from {base}");
        let target = diff::Target::Branch {
            path: path.clone(),
            base,
        };
        self.next_id += 1;
        let view = DiffView::new(self.next_id, title, target, self.worktree_stamp(&Key::Worktree(path)));
        self.diff = Some(view);
        self.spawn_diff_load();
    }

    /// `o`: what worktree `wi` and the worktrees it overlaps with each did to their shared files.
    fn open_overlaps(&mut self, wi: usize) {
        let Some(w) = self.snap.worktrees.get(wi).filter(|w| w.has_worktree()) else {
            return;
        };
        if w.overlaps.is_empty() {
            self.notice = Some(format!("[{}] doesn't overlap with other worktrees", w.name));
            return;
        }
        let names: Vec<&str> = w.overlaps.iter().map(|o| o.with.as_str()).collect();
        let title = format!("[{}] overlaps with {}", w.name, names.join(", "));
        let target = diff::Target::Overlaps {
            path: w.path.clone(),
            name: w.name.clone(),
            overlaps: w.overlaps.clone(),
        };
        let stamp = self.worktree_stamp(&Key::Worktree(w.path.clone()));
        self.next_id += 1;
        self.diff = Some(DiffView::new(self.next_id, title, target, stamp));
        self.spawn_diff_load();
    }

    fn open_shell_inset(&mut self, path: &Path, command: Option<String>) {
        if self.shell.is_some() {
            return;
        }
        self.next_id += 1;
        let id = self.next_id;
        let (out, exit) = (self.tx.clone(), self.tx.clone());
        // Sized properly on the next render.
        let started = term::Term::spawn(
            path,
            command.as_deref(),
            12,
            80,
            move || {
                let _ = out.send(UiMsg::TermOutput);
            },
            move || {
                let _ = exit.send(UiMsg::TermExited(id));
            },
        );
        match started {
            Ok(term) => {
                let label = match &command {
                    Some(c) => program_name(c),
                    None => "shell".into(),
                };
                self.shell = Some(Shell { id, label, term });
                self.shell_focus = true;
            }
            Err(e) => self.notice = Some(format!("can't start a shell: {e}")),
        }
    }

    /// The panel and screen row under the pointer, when it is over a line of a panel's content.
    fn hover_at(&self, pos: Position) -> Option<(Focus, u16)> {
        if self.shell.is_some() && self.areas.shell.contains(pos) {
            return None;
        }
        [(Focus::Worktrees, self.areas.worktrees), (Focus::Graph, self.areas.graph)]
            .into_iter()
            .find(|(_, a)| render::content(*a).contains(pos))
            .map(|(f, _)| (f, pos.y))
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        self.hover = self.hover_at(pos);
        if let Some(shell) = self.shell.as_mut()
            && self.areas.shell.contains(pos)
        {
            match m.kind {
                MouseEventKind::Down(_) => self.shell_focus = true,
                MouseEventKind::ScrollUp => shell.term.scroll(3),
                MouseEventKind::ScrollDown => shell.term.scroll(-3),
                _ => {}
            }
            return;
        }
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.shell_focus = false;
            self.notice = None;
        }
        let hit = [
            (Focus::Worktrees, self.areas.worktrees),
            (Focus::Graph, self.areas.graph),
        ]
        .into_iter()
        .find(|(_, a)| a.contains(pos));
        let Some((f, area)) = hit else { return };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let was_focused = self.focus == f;
                self.focus = f;
                let inner = render::content(area);
                if !inner.contains(pos) {
                    return; // the title rule or a side margin
                }
                let idx = self.panel(f).pos.offset + (m.row - inner.y) as usize;
                if idx >= self.panel(f).lines.len() {
                    return;
                }
                // The first click selects; a click on the line already selected (in the focused
                // panel) expands/collapses it.
                let selected = was_focused && self.panel(f).pos.sel == idx;
                self.set_cursor(f, idx);
                if f == Focus::Worktrees {
                    // Always, also when the graph cursor was moved away from this worktree.
                    self.jump_graph_to_selected();
                }
                let now = Instant::now();
                let double = self
                    .last_click
                    .as_ref()
                    .filter(|c| c.panel == f && c.line == idx && now.duration_since(c.at) < Duration::from_millis(400));
                if let Some(first) = double {
                    // Double-click: undo the first click's expand/collapse, then act on what was
                    // clicked: a worktree's agent marker or path, else open the full view.
                    if let Some((tf, tl)) = first.toggled {
                        self.toggle_at(tf, tl);
                    }
                    self.last_click = None;
                    match self.wt_field_at(f, idx, m.column - inner.x) {
                        Some(WtField::Agent) => self.focus_agent(),
                        Some(WtField::Path(path)) => self.open_shell(path, None),
                        Some(WtField::Overlap(wi)) => self.open_overlaps(wi),
                        None => self.open_full(f, idx),
                    }
                } else {
                    let toggled = if !selected {
                        None
                    } else if f == Focus::Worktrees {
                        // Like Space in the list: act on the worktree's row in the graph.
                        let on_row = self.graph_cursor_to_selected_worktree();
                        let l = self.graph.pos.sel;
                        (on_row && self.toggle_at(Focus::Graph, l)).then_some((Focus::Graph, l))
                    } else {
                        self.toggle_at(f, idx).then_some((f, idx))
                    };
                    self.last_click = Some(Click {
                        at: now,
                        panel: f,
                        line: idx,
                        toggled,
                    });
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d: isize = if m.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
                let h = render::content(area).height as usize;
                let p = self.panel_mut(f);
                p.pos.scroll_by(d, p.lines.len(), h);
                if f == Focus::Graph {
                    self.maybe_load_more();
                }
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // Full-screen diff view

    /// Open the full diff view for the thing at `line`, focused on the file (and position)
    /// under the cursor when it is inside an inset.
    fn open_full(&mut self, f: Focus, line: usize) {
        let Some(&l) = self.panel(f).lines.get(line) else {
            return;
        };
        let r = l.row();
        let Some(key) = self.target(f, r) else { return };
        let Some(target) = self.diff_target(&key) else { return };
        let title = match &key {
            Key::Commit(id) => {
                let short = &id[..id.len().min(7)];
                let g = &self.snap.graph;
                let subject = g.commit_row.get(id).and_then(|&row| match g.rows[row].kind {
                    RowKind::Commit(ci) => Some(g.commits[ci].subject.clone()),
                    RowKind::Uncommitted(_) => None,
                });
                match subject {
                    Some(s) => format!("{short} {s}"),
                    None => short.to_string(),
                }
            }
            Key::Worktree(p) => {
                let name = self
                    .snap
                    .worktrees
                    .iter()
                    .find(|w| &w.path == p)
                    .map(|w| w.name.as_str());
                format!("[{}] uncommitted changes", name.unwrap_or("?"))
            }
        };
        // Where to land: (file, scroll) within the doc.
        let focus = match l {
            VLine::Row(_) => None,
            VLine::Inset(_, Item::Subject(_) | Item::Message(_)) => Some((None, 0)),
            VLine::Inset(_, Item::File(fi)) => self.file_path(f, r, fi).map(|p| (Some(p), 0)),
            VLine::Inset(_, Item::Diff(fi, li)) => self.file_path(f, r, fi).map(|p| (Some(p), li.saturating_sub(3))),
            VLine::Inset(_, Item::More(fi, _)) => self.row_doc(f, r).and_then(|e| {
                let file = e.ok()?.files.get(fi)?;
                Some((Some(file.path.clone()), inset::diff_start(file) + inset::MAX_INLINE - 3))
            }),
            VLine::Inset(_, _) => None,
        };

        self.next_id += 1;
        let mut view = DiffView::new(self.next_id, title, target, self.worktree_stamp(&key));
        if let Some((file, scroll)) = focus {
            view.focus_on(file, scroll);
        }
        let cached = self.docs.get(&key).filter(|e| self.worktree_stamp(&key) == e.stamp);
        match cached.and_then(|e| e.ok()) {
            Some(doc) => {
                view.loaded(Ok(doc.clone()));
                self.diff = Some(view);
            }
            None => {
                self.diff = Some(view);
                self.spawn_diff_load();
            }
        }
    }

    fn reload_diff(&mut self) {
        let stamp = match &self.diff {
            Some(d) => d
                .target
                .worktree()
                .and_then(|p| self.worktree_stamp(&Key::Worktree(p.to_path_buf()))),
            None => return,
        };
        self.next_id += 1;
        if let Some(d) = self.diff.as_mut() {
            d.begin_reload(self.next_id);
            if stamp.is_some() {
                d.stamp = stamp;
            }
        }
        self.spawn_diff_load();
    }

    fn spawn_diff_load(&self) {
        let Some(d) = &self.diff else { return };
        let (id, target, tx) = (d.id, d.target.clone(), self.tx.clone());
        spawn_named("canopy-diff", move || {
            let result = diff::load(&target).map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiMsg::Diff { id, result });
        });
    }

    fn diff_action(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Close => self.diff = None,
            Action::Reload => self.reload_diff(),
            Action::Help => self.help = true,
            Action::Quit => self.quit = true,
        }
    }
}

enum WtField {
    Agent,
    Path(PathBuf),
    Overlap(usize),
}

/// A shell in `path` in a tmux pane split below canopy's, when canopy runs inside tmux. tmux
/// starts the default shell there (or `command`, closing the pane when it exits) and focuses it;
/// `false` if tmux couldn't split.
fn tmux_split(path: &Path, command: Option<&str>) -> bool {
    if std::env::var_os("TMUX").is_none() {
        return false;
    }
    let mut cmd = std::process::Command::new("tmux");
    cmd.args(["split-window", "-v", "-c"]).arg(path);
    // Split canopy's own pane, not whichever pane happens to be active.
    if let Some(pane) = std::env::var_os("TMUX_PANE") {
        cmd.arg("-t").arg(pane);
    }
    if let Some(c) = command {
        cmd.arg(c);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Git TUIs `t` looks for on PATH, in order.
const GIT_TUIS: [&str; 3] = ["lazygit", "gitui", "tig"];

/// The first of [`GIT_TUIS`] on PATH.
fn find_git_tui() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    GIT_TUIS
        .into_iter()
        .find(|name| {
            std::env::split_paths(&path).any(|dir| {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(dir.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
        })
        .map(str::to_string)
}

/// `lazygit` for `/usr/bin/lazygit -p x`: what the inset's title calls it.
fn program_name(command: &str) -> String {
    let first = command.split_whitespace().next().unwrap_or(command);
    Path::new(first)
        .file_name()
        .map_or_else(|| first.to_string(), |n| n.to_string_lossy().into_owned())
}

fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) {
    thread::Builder::new().name(name.into()).spawn(f).expect("spawn thread");
}

/// Run the TUI until the user quits.
pub fn run(paths: RepoPaths, cfg: Config, layout: LayoutMode, with_herdr: bool, git_tui: Option<String>) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    spawn_input(tx.clone());
    if with_herdr {
        let herdr_tx = tx.clone();
        herdr::spawn(move |view| {
            let _ = herdr_tx.send(UiMsg::Herdr(view));
        });
    }
    let snap_tx = tx.clone();
    let worker = watch::spawn(paths, cfg.clone(), move |s| {
        let _ = snap_tx.send(UiMsg::Snapshot(s));
    });

    // Disable mouse capture on panic too; ratatui's own hook restores the rest.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        prev(info);
    }));
    let mut terminal = ratatui::try_init()?;
    let _ = execute!(stdout(), EnableMouseCapture);

    let mut app = App::new(worker, tx, &cfg, layout);
    app.with_herdr = with_herdr;
    app.herdr_dir = herdr::worktree_dir();
    app.git_tui = git_tui;
    let res = event_loop(&mut terminal, &mut app, &rx);

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App, rx: &Receiver<UiMsg>) -> Result<()> {
    let mut draw = true;
    loop {
        if draw {
            terminal.draw(|f| render::draw(f, app))?;
        }
        let Ok(msg) = rx.recv() else { return Ok(()) };
        app.redraw = true;
        app.handle(msg);
        draw = app.redraw;
        // Coalesce whatever else is queued so bursts of input or snapshots cost one redraw.
        while !app.quit
            && let Ok(msg) = rx.try_recv()
        {
            app.redraw = true;
            app.handle(msg);
            draw |= app.redraw;
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn spawn_input(tx: Sender<UiMsg>) {
    spawn_named("canopy-input", move || {
        while let Ok(ev) = event::read() {
            if tx.send(UiMsg::Input(ev)).is_err() {
                break;
            }
        }
    });
}
