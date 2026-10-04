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

/// A vertically scrolling selection over `len` rows.
#[derive(Clone, Copy, Debug, Default)]
pub struct ListPos {
    pub sel: usize,
    pub offset: usize,
}

impl ListPos {
    pub fn clamp(&mut self, len: usize) {
        self.sel = self.sel.min(len.saturating_sub(1));
        self.offset = self.offset.min(len.saturating_sub(1));
    }

    pub fn move_by(&mut self, delta: isize, len: usize) {
        let max = len.saturating_sub(1) as isize;
        self.sel = (self.sel as isize + delta).clamp(0, max.max(0)) as usize;
    }

    /// Adjust the offset so the selection is visible in `height` rows.
    pub fn scroll_into_view(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.sel < self.offset {
            self.offset = self.sel;
        } else if self.sel >= self.offset + height {
            self.offset = self.sel + 1 - height;
        }
    }
}

pub struct App {
    pub snap: Arc<Snapshot>,
    pub start: PathBuf,
    pub interval_ms: u128,
    pub focus: Focus,
    pub wt: ListPos,
    pub graph: ListPos,
    pub help: bool,
    pub quit: bool,
    pub areas: Areas,
    worker: watch::Handle,
}

impl App {
    fn new(worker: watch::Handle, start: PathBuf, cfg: &Config) -> Self {
        App {
            snap: Arc::new(Snapshot::default()),
            start,
            interval_ms: cfg.interval.as_millis(),
            focus: Focus::Worktrees,
            wt: ListPos::default(),
            graph: ListPos::default(),
            help: false,
            quit: false,
            areas: Areas::default(),
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

    fn on_snapshot(&mut self, snap: Arc<Snapshot>) {
        let first = self.snap.worktrees.is_empty();
        let selected = self.snap.worktrees.get(self.wt.sel).map(|w| w.path.clone());
        self.snap = snap;
        if first {
            // Start on the worktree canopy was launched from.
            if let Some(i) = self.snap.worktrees.iter().position(|w| w.is_current) {
                self.wt.sel = i;
            }
        } else if let Some(p) = selected
            && let Some(i) = self.snap.worktrees.iter().position(|w| w.path == p)
        {
            self.wt.sel = i;
        }
        self.wt.clamp(self.snap.worktrees.len());
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
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX / 2),
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
                self.wt.move_by(delta, self.snap.worktrees.len());
            }
            Focus::Graph => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        let in_wt = self.areas.worktrees.contains(pos);
        let in_graph = self.areas.graph.contains(pos);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if in_wt {
                    self.focus = Focus::Worktrees;
                    let row = m.row.saturating_sub(self.areas.worktrees.y + 1) as usize;
                    let i = self.wt.offset + row;
                    if m.row > self.areas.worktrees.y && i < self.snap.worktrees.len() {
                        self.wt.sel = i;
                    }
                } else if in_graph {
                    self.focus = Focus::Graph;
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d = if m.kind == MouseEventKind::ScrollDown {
                    1
                } else {
                    -1
                };
                if in_wt {
                    self.focus = Focus::Worktrees;
                    self.move_sel(d);
                }
            }
            _ => {}
        }
    }
}

/// Run the TUI until the user quits.
pub fn run(paths: RepoPaths, start: PathBuf, cfg: Config) -> Result<()> {
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

    let mut app = App::new(worker, start, &cfg);
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
        while let Ok(msg) = rx.try_recv() {
            app.handle(msg);
            if app.quit {
                break;
            }
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
