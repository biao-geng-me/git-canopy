//! Terminal UI. Runs on the main thread and only renders snapshots received over a channel;
//! all git work happens on the refresh worker (and on short-lived diff threads).

mod render;

use std::io::stdout;
use std::path::PathBuf;
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

use crate::diff;
use crate::git::RepoPaths;
use crate::git::status::Status;
use crate::graph::RowKind;
use crate::model::Snapshot;
use crate::watch::{self, Config, Request};

pub enum UiMsg {
    Input(Event),
    Snapshot(Arc<Snapshot>),
    Diff {
        id: u64,
        result: Result<Vec<String>, String>,
    },
}

/// The full-screen diff view.
pub struct DiffView {
    pub id: u64,
    pub title: String,
    pub target: diff::Target,
    /// `None` while loading.
    pub lines: Option<Result<Vec<String>, String>>,
    pub scroll: usize,
    pub hscroll: usize,
    /// For worktree diffs: the status the diff was loaded with; reloaded when it changes.
    pub status: Option<Status>,
    /// Keep the scroll position across the next load (reloads), instead of starting at the top.
    keep_scroll: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Worktrees,
    Graph,
}

/// Screen regions from the last render, for mouse hit-testing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Areas {
    pub worktrees: Rect,
    pub graph: Rect,
    pub diff: Rect,
}

/// A vertically scrolling selection over a list of rows.
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

/// Identity of a graph row, stable across refreshes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowKey {
    Commit(String),
    Worktree(PathBuf),
}

pub struct App {
    pub snap: Arc<Snapshot>,
    pub interval_ms: u128,
    pub focus: Focus,
    pub wt: ListPos,
    pub graph: ListPos,
    /// Pending "show this worktree in the graph" request, applied at render time (needs height).
    pub graph_jump: Option<PathBuf>,
    pub diff: Option<DiffView>,
    pub help: bool,
    pub quit: bool,
    pub areas: Areas,
    next_diff_id: u64,
    tx: Sender<UiMsg>,
    /// Last left click (time, panel, row), for double-click detection.
    last_click: Option<(Instant, Focus, usize)>,
    /// Graph row count when "load more" was last requested, to ask only once per page.
    more_requested_at: usize,
    worker: watch::Handle,
}

impl App {
    fn new(worker: watch::Handle, tx: Sender<UiMsg>, cfg: &Config) -> Self {
        App {
            snap: Arc::new(Snapshot::default()),
            interval_ms: cfg.interval.as_millis(),
            focus: Focus::Worktrees,
            wt: ListPos::default(),
            graph: ListPos::default(),
            graph_jump: None,
            diff: None,
            help: false,
            quit: false,
            areas: Areas::default(),
            next_diff_id: 0,
            tx,
            last_click: None,
            more_requested_at: 0,
            worker,
        }
    }

    fn handle(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Snapshot(s) => self.on_snapshot(s),
            UiMsg::Diff { id, result } => {
                if let Some(d) = self.diff.as_mut().filter(|d| d.id == id) {
                    if !d.keep_scroll {
                        d.scroll = 0;
                    }
                    d.keep_scroll = false;
                    d.lines = Some(result);
                    self.scroll_diff(0);
                }
            }
            UiMsg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                if self.diff.is_some() && !self.help {
                    self.on_diff_key(k)
                } else {
                    self.on_key(k)
                }
            }
            UiMsg::Input(Event::Mouse(m)) => {
                if self.diff.is_some() {
                    self.on_diff_mouse(m)
                } else {
                    self.on_mouse(m)
                }
            }
            UiMsg::Input(_) => {}
        }
    }

    fn row_key(&self, row: usize) -> Option<RowKey> {
        let g = &self.snap.graph;
        Some(match g.rows.get(row)?.kind {
            RowKind::Commit(ci) => RowKey::Commit(g.commits[ci].id.clone()),
            RowKind::Uncommitted(wi) => RowKey::Worktree(self.snap.worktrees.get(wi)?.path.clone()),
        })
    }

    fn on_snapshot(&mut self, snap: Arc<Snapshot>) {
        let first_wts = self.snap.worktrees.is_empty();
        let first_graph = self.snap.graph.rows.is_empty();
        let selected_wt = self.snap.worktrees.get(self.wt.sel).map(|w| w.path.clone());
        let selected_row = self.row_key(self.graph.sel);
        let old_graph = self.snap.graph.clone();
        self.snap = snap;

        if first_wts {
            // Start on the worktree canopy was launched from.
            if let Some(i) = self.snap.worktrees.iter().position(|w| w.is_current) {
                self.wt.sel = i;
            }
        } else if let Some(p) = selected_wt
            && let Some(i) = self.snap.worktrees.iter().position(|w| w.path == p)
        {
            self.wt.sel = i;
        }
        self.wt.clamp(self.snap.worktrees.len());

        if first_graph && !self.snap.graph.rows.is_empty() {
            self.graph_jump = self.snap.worktrees.get(self.wt.sel).map(|w| w.path.clone());
        } else if !Arc::ptr_eq(&old_graph, &self.snap.graph) {
            self.relocate_graph_selection(selected_row, &old_graph);
        }
        self.graph.clamp(self.snap.graph.rows.len());

        // Keep an open worktree diff in sync when that worktree's status changes.
        let changed = match &self.diff {
            Some(DiffView {
                target: diff::Target::Worktree { path, .. },
                status,
                lines: Some(_),
                ..
            }) => {
                let now = self
                    .snap
                    .worktrees
                    .iter()
                    .find(|w| &w.path == path)
                    .and_then(|w| w.status.clone());
                now.is_some() && now != *status
            }
            _ => false,
        };
        if changed {
            self.reload_diff();
        }
    }

    /// Keep the same commit (or uncommitted row) selected when the graph is rebuilt.
    fn relocate_graph_selection(&mut self, key: Option<RowKey>, old: &crate::graph::Graph) {
        let g = &self.snap.graph;
        let found = match &key {
            Some(RowKey::Commit(id)) => g.commit_row.get(id).copied(),
            Some(RowKey::Worktree(p)) => g
                .rows
                .iter()
                .position(|r| matches!(r.kind, RowKind::Uncommitted(wi) if self.snap.worktrees.get(wi).is_some_and(|w| &w.path == p)))
                // The worktree became clean: fall back to its HEAD.
                .or_else(|| g.worktree_row.get(p).copied()),
            None => None,
        };
        if let Some(row) = found {
            // Shift the viewport with the row so rows inserted above don't make the view jump.
            let shift = row as isize - self.graph.sel as isize;
            self.graph.offset = (self.graph.offset as isize + shift).max(0) as usize;
            self.graph.sel = row;
        } else if old.rows.is_empty() {
            self.graph.sel = 0;
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.help {
            // Any key closes the help popup; Ctrl-C still quits.
            self.help = false;
            if !(ctrl && k.code == KeyCode::Char('c')) {
                return;
            }
        }
        match k.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => self.worker.request(Request::Refresh),
            KeyCode::Enter | KeyCode::Char(' ') => self.open_diff(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Worktrees => Focus::Graph,
                    Focus::Graph => Focus::Worktrees,
                }
            }
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Worktrees,
            KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Graph,
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
        (area.height.saturating_sub(2) as usize).max(1)
    }

    fn move_sel(&mut self, delta: isize) {
        match self.focus {
            Focus::Worktrees => {
                let before = self.wt.sel;
                self.wt.move_by(delta, self.snap.worktrees.len());
                if self.wt.sel != before {
                    self.select_worktree(self.wt.sel);
                }
            }
            Focus::Graph => {
                self.graph.move_by(delta, self.snap.graph.rows.len());
                self.maybe_load_more();
            }
        }
    }

    /// Select a worktree and show it in the graph.
    fn select_worktree(&mut self, i: usize) {
        self.wt.sel = i;
        self.wt.follow = true;
        self.graph_jump = self.snap.worktrees.get(i).map(|w| w.path.clone());
    }

    /// Ask the worker for more history when the selection nears the end of what is loaded.
    fn maybe_load_more(&mut self) {
        let g = &self.snap.graph;
        let rows = g.rows.len();
        let near_end = self.graph.sel + 2 * self.page() >= rows || self.graph.offset + 2 * self.page() >= rows;
        if g.truncated && near_end && self.more_requested_at != rows {
            self.more_requested_at = rows;
            self.worker.request(Request::LoadMore);
        }
    }

    /// Register a click on a row; a second click on the same row soon after opens its diff.
    fn click(&mut self, panel: Focus, row: usize) {
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(t, p, r)| p == panel && r == row && now.duration_since(t) < Duration::from_millis(400));
        self.last_click = if double { None } else { Some((now, panel, row)) };
        if double {
            self.open_diff();
        }
    }

    /// Open the diff for the current selection: a commit, or a worktree's uncommitted changes.
    /// A clean worktree shows its HEAD commit instead.
    fn open_diff(&mut self) {
        let snap = self.snap.clone();
        let commit = |id: &str| {
            let subject = snap
                .graph
                .commit_row
                .get(id)
                .and_then(|&r| match snap.graph.rows[r].kind {
                    RowKind::Commit(ci) => Some(snap.graph.commits[ci].subject.clone()),
                    RowKind::Uncommitted(_) => None,
                });
            let short = &id[..id.len().min(7)];
            let title = match subject {
                Some(s) => format!("{short} {s}"),
                None => short.to_string(),
            };
            (
                title,
                diff::Target::Commit {
                    dir: snap.common_dir.clone(),
                    id: id.to_string(),
                },
                None,
            )
        };
        let worktree = |wi: usize| {
            let w = snap.worktrees.get(wi)?;
            if !w.has_worktree() {
                return None;
            }
            if !w.is_dirty() && w.status.is_some() {
                return w.head.as_deref().map(commit);
            }
            let title = format!("[{}] uncommitted changes", w.name);
            let target = diff::Target::Worktree {
                path: w.path.clone(),
                has_head: w.head.is_some(),
            };
            Some((title, target, w.status.clone()))
        };
        let picked = match self.focus {
            Focus::Worktrees => worktree(self.wt.sel),
            Focus::Graph => match snap.graph.rows.get(self.graph.sel).map(|r| r.kind) {
                Some(RowKind::Commit(ci)) => Some(commit(&snap.graph.commits[ci].id)),
                Some(RowKind::Uncommitted(wi)) => worktree(wi),
                None => None,
            },
        };
        let Some((title, target, status)) = picked else { return };
        self.next_diff_id += 1;
        self.diff = Some(DiffView {
            id: self.next_diff_id,
            title,
            target,
            lines: None,
            scroll: 0,
            hscroll: 0,
            status,
            keep_scroll: false,
        });
        self.spawn_diff_load();
    }

    fn reload_diff(&mut self) {
        let status = match &self.diff {
            Some(DiffView {
                target: diff::Target::Worktree { path, .. },
                ..
            }) => self
                .snap
                .worktrees
                .iter()
                .find(|w| &w.path == path)
                .and_then(|w| w.status.clone()),
            _ => None,
        };
        if let Some(d) = self.diff.as_mut() {
            self.next_diff_id += 1;
            d.id = self.next_diff_id;
            d.keep_scroll = true;
            if status.is_some() {
                d.status = status;
            }
        }
        self.spawn_diff_load();
    }

    fn spawn_diff_load(&self) {
        let Some(d) = &self.diff else { return };
        let (id, target, tx) = (d.id, d.target.clone(), self.tx.clone());
        thread::Builder::new()
            .name("canopy-diff".into())
            .spawn(move || {
                let result = diff::load(&target).map_err(|e| format!("{e:#}"));
                let _ = tx.send(UiMsg::Diff { id, result });
            })
            .expect("spawn diff thread");
    }

    fn diff_page(&self) -> usize {
        (self.areas.diff.height.saturating_sub(2) as usize).max(1)
    }

    fn on_diff_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.diff_page() as isize;
        match k.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => self.diff = None,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => self.reload_diff(),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Enter => self.scroll_diff(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_diff(-1),
            KeyCode::Char('d') if ctrl => self.scroll_diff(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_diff(-page / 2),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => self.scroll_diff(page),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_diff(-page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_diff(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.scroll_diff(isize::MAX),
            KeyCode::Right | KeyCode::Char('l') => self.pan_diff(8),
            KeyCode::Left | KeyCode::Char('h') => self.pan_diff(-8),
            KeyCode::Char('n') | KeyCode::Char(']') => self.jump_file(true),
            KeyCode::Char('p') | KeyCode::Char('N') | KeyCode::Char('[') => self.jump_file(false),
            _ => {}
        }
    }

    fn on_diff_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll_diff(3),
            MouseEventKind::ScrollUp => self.scroll_diff(-3),
            MouseEventKind::ScrollRight => self.pan_diff(8),
            MouseEventKind::ScrollLeft => self.pan_diff(-8),
            _ => {}
        }
    }

    fn scroll_diff(&mut self, delta: isize) {
        let page = self.diff_page();
        let Some(d) = self.diff.as_mut() else { return };
        let len = d.lines.as_ref().and_then(|r| r.as_ref().ok()).map_or(0, Vec::len);
        let max = len.saturating_sub(page) as isize;
        d.scroll = (d.scroll as isize).saturating_add(delta).clamp(0, max.max(0)) as usize;
    }

    fn pan_diff(&mut self, delta: isize) {
        if let Some(d) = self.diff.as_mut() {
            d.hscroll = (d.hscroll as isize + delta).clamp(0, 10_000) as usize;
        }
    }

    fn jump_file(&mut self, forward: bool) {
        let Some(d) = self.diff.as_mut() else { return };
        let Some(Ok(lines)) = &d.lines else { return };
        if let Some(i) = diff::file_jump(lines, d.scroll, forward) {
            d.scroll = i;
        } else if !forward {
            d.scroll = 0;
        }
        self.scroll_diff(0);
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        let (wa, ga) = (self.areas.worktrees, self.areas.graph);
        // Row index inside a bordered panel, if the position is on a content row.
        let row_in = |area: Rect, offset: usize| -> Option<usize> {
            (area.contains(pos) && m.row > area.y && m.row + 1 < area.y + area.height)
                .then(|| offset + (m.row - area.y - 1) as usize)
        };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if wa.contains(pos) {
                    self.focus = Focus::Worktrees;
                    if let Some(i) = row_in(wa, self.wt.offset).filter(|&i| i < self.snap.worktrees.len()) {
                        self.select_worktree(i);
                        self.click(Focus::Worktrees, i);
                    }
                } else if ga.contains(pos) {
                    self.focus = Focus::Graph;
                    if let Some(i) = row_in(ga, self.graph.offset).filter(|&i| i < self.snap.graph.rows.len()) {
                        self.graph.sel = i;
                        self.click(Focus::Graph, i);
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d: isize = if m.kind == MouseEventKind::ScrollDown { 1 } else { -1 };
                if wa.contains(pos) {
                    self.focus = Focus::Worktrees;
                    self.move_sel(d);
                } else if ga.contains(pos) {
                    let h = ga.height.saturating_sub(2) as usize;
                    self.graph.scroll_by(3 * d, self.snap.graph.rows.len(), h);
                    self.maybe_load_more();
                }
            }
            _ => {}
        }
    }
}

/// Run the TUI until the user quits.
pub fn run(paths: RepoPaths, cfg: Config) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    spawn_input(tx.clone());
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

    let mut app = App::new(worker, tx, &cfg);
    let res = event_loop(&mut terminal, &mut app, &rx);

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App, rx: &Receiver<UiMsg>) -> Result<()> {
    loop {
        terminal.draw(|f| render::draw(f, app))?;
        let Ok(msg) = rx.recv() else { return Ok(()) };
        app.handle(msg);
        // Coalesce whatever else is queued so bursts of input or snapshots cost one redraw.
        while !app.quit
            && let Ok(msg) = rx.try_recv()
        {
            app.handle(msg);
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn spawn_input(tx: Sender<UiMsg>) {
    thread::Builder::new()
        .name("canopy-input".into())
        .spawn(move || {
            while let Ok(ev) = event::read() {
                if tx.send(UiMsg::Input(ev)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn input thread");
}
