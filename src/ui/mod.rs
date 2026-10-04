//! Terminal UI. Runs on the main thread and only renders snapshots received over a channel;
//! all git work happens on the refresh worker (and on short-lived diff threads).

mod render;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Position, Rect};

use crate::git::RepoPaths;
use crate::graph::RowKind;
use crate::model::Snapshot;
use crate::watch::{self, Config, Request};

pub enum UiMsg {
    Input(Event),
    Snapshot(Arc<Snapshot>),
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
        self.sel = (self.sel as isize)
            .saturating_add(delta)
            .clamp(0, max.max(0)) as usize;
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
    pub help: bool,
    pub quit: bool,
    pub areas: Areas,
    /// Graph row count when "load more" was last requested, to ask only once per page.
    more_requested_at: usize,
    worker: watch::Handle,
}

impl App {
    fn new(worker: watch::Handle, cfg: &Config) -> Self {
        App {
            snap: Arc::new(Snapshot::default()),
            interval_ms: cfg.interval.as_millis(),
            focus: Focus::Worktrees,
            wt: ListPos::default(),
            graph: ListPos::default(),
            graph_jump: None,
            help: false,
            quit: false,
            areas: Areas::default(),
            more_requested_at: 0,
            worker,
        }
    }

    fn handle(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Snapshot(s) => self.on_snapshot(s),
            UiMsg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => self.on_key(k),
            UiMsg::Input(Event::Mouse(m)) => self.on_mouse(m),
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
            self.help = false;
            if !matches!(k.code, KeyCode::Char('q')) || ctrl {
                return;
            }
        }
        match k.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => self.worker.request(Request::Refresh),
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
        let near_end =
            self.graph.sel + 2 * self.page() >= rows || self.graph.offset + 2 * self.page() >= rows;
        if g.truncated && near_end && self.more_requested_at != rows {
            self.more_requested_at = rows;
            self.worker.request(Request::LoadMore);
        }
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
                    if let Some(i) =
                        row_in(wa, self.wt.offset).filter(|&i| i < self.snap.worktrees.len())
                    {
                        self.select_worktree(i);
                    }
                } else if ga.contains(pos) {
                    self.focus = Focus::Graph;
                    if let Some(i) =
                        row_in(ga, self.graph.offset).filter(|&i| i < self.snap.graph.rows.len())
                    {
                        self.graph.sel = i;
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d: isize = if m.kind == MouseEventKind::ScrollDown {
                    1
                } else {
                    -1
                };
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

    let mut app = App::new(worker, &cfg);
    let res = event_loop(&mut terminal, &mut app, &rx);

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &Receiver<UiMsg>,
) -> Result<()> {
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
