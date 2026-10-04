//! The diff view: a file list on top (overview + one entry per file), the selected entry's diff
//! below. State and input handling only; drawing lives in `render`.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::diff::{self, DiffDoc, FileDiff};
use crate::git::status::Status;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Files,
    Body,
}

/// What the app should do after the view handled an input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    Reload,
    Help,
    Quit,
}

pub struct DiffView {
    pub id: u64,
    pub title: String,
    pub target: diff::Target,
    /// For worktree diffs: the (status, last edit) the diff was loaded for; reloaded when it changes.
    pub stamp: Option<(Option<Status>, Option<i64>)>,
    /// `None` while loading.
    pub doc: Option<Result<DiffDoc, String>>,
    /// Selected entry: 0 is the overview, `i` is `files[i - 1]`.
    pub sel: usize,
    pub list_offset: usize,
    pub scroll: usize,
    pub hscroll: usize,
    pub pane: Pane,
    /// Regions from the last render, for paging and mouse hit-testing.
    pub list_area: Rect,
    pub body_area: Rect,
    /// On reload: the entry to keep selected (path, or `None` for the overview) and its scroll.
    restore: Option<(Option<String>, usize)>,
}

impl DiffView {
    pub fn new(id: u64, title: String, target: diff::Target, stamp: Option<(Option<Status>, Option<i64>)>) -> Self {
        DiffView {
            id,
            title,
            target,
            stamp,
            doc: None,
            sel: 0,
            list_offset: 0,
            scroll: 0,
            hscroll: 0,
            pane: Pane::Files,
            list_area: Rect::default(),
            body_area: Rect::default(),
            restore: None,
        }
    }

    pub fn files(&self) -> &[FileDiff] {
        match &self.doc {
            Some(Ok(d)) => &d.files,
            _ => &[],
        }
    }

    pub fn entries(&self) -> usize {
        1 + self.files().len()
    }

    pub fn has_files(&self) -> bool {
        !self.files().is_empty()
    }

    pub fn selected_file(&self) -> Option<&FileDiff> {
        self.sel.checked_sub(1).and_then(|i| self.files().get(i))
    }

    /// Lines of the selected entry.
    pub fn body(&self) -> &[String] {
        match (&self.doc, self.selected_file()) {
            (_, Some(f)) => &f.lines,
            (Some(Ok(d)), None) => &d.overview,
            _ => &[],
        }
    }

    pub fn is_commit(&self) -> bool {
        matches!(self.target, diff::Target::Commit { .. })
    }

    /// Land on `file` (`None` = overview) scrolled to `scroll` once loaded, with the diff focused.
    pub fn focus_on(&mut self, file: Option<String>, scroll: usize) {
        self.restore = Some((file, scroll));
        self.pane = Pane::Body;
    }

    /// Mark the view as reloading under a new request id, remembering the current position.
    pub fn begin_reload(&mut self, id: u64) {
        self.id = id;
        let key = self.selected_file().map(|f| f.path.clone());
        self.restore = Some((key, self.scroll));
    }

    pub fn loaded(&mut self, result: Result<DiffDoc, String>) {
        self.doc = Some(result);
        match self.restore.take() {
            Some((key, scroll)) => {
                let found = match &key {
                    None => Some(0),
                    Some(p) => self.files().iter().position(|f| &f.path == p).map(|i| i + 1),
                };
                match found {
                    Some(i) => {
                        self.sel = i;
                        self.scroll = scroll;
                    }
                    None => {
                        self.sel = self.sel.min(self.entries() - 1);
                        self.scroll = 0;
                    }
                }
            }
            None => {
                // Start on the first file; the overview is one step up.
                self.sel = usize::from(self.has_files());
                self.scroll = 0;
            }
        }
        if !self.has_files() {
            self.pane = Pane::Body;
        }
        self.scroll_by(0);
    }

    fn select(&mut self, i: usize) {
        let i = i.min(self.entries() - 1);
        if i != self.sel {
            self.sel = i;
            self.scroll = 0;
            self.hscroll = 0;
        }
    }

    fn select_by(&mut self, delta: isize) {
        let max = self.entries() as isize - 1;
        self.select((self.sel as isize).saturating_add(delta).clamp(0, max) as usize);
    }

    fn page(&self) -> isize {
        (super::render::content(self.body_area).height as isize).max(1)
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.body().len().saturating_sub(self.page() as usize) as isize;
        self.scroll = (self.scroll as isize).saturating_add(delta).clamp(0, max.max(0)) as usize;
    }

    fn pan(&mut self, delta: isize) {
        self.hscroll = (self.hscroll as isize + delta).clamp(0, 10_000) as usize;
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.page();
        let in_list = self.pane == Pane::Files && self.has_files();
        match k.code {
            KeyCode::Char('c') if ctrl => return Action::Quit,
            KeyCode::Char('q') | KeyCode::Backspace => return Action::Close,
            KeyCode::Esc if self.pane == Pane::Body && self.has_files() => self.pane = Pane::Files,
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('?') => return Action::Help,
            KeyCode::Char('r') => return Action::Reload,
            KeyCode::Tab | KeyCode::BackTab if self.has_files() => {
                self.pane = if self.pane == Pane::Files {
                    Pane::Body
                } else {
                    Pane::Files
                };
            }
            KeyCode::Enter if in_list => self.pane = Pane::Body,
            KeyCode::Down | KeyCode::Char('j') if in_list => self.select_by(1),
            KeyCode::Up | KeyCode::Char('k') if in_list => self.select_by(-1),
            KeyCode::Home | KeyCode::Char('g') if in_list => self.select(0),
            KeyCode::End | KeyCode::Char('G') if in_list => self.select(usize::MAX),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Enter => self.scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_by(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.scroll_by(isize::MAX),
            KeyCode::Char('n') | KeyCode::Char(']') => self.select_by(1),
            KeyCode::Char('p') | KeyCode::Char('N') | KeyCode::Char('[') => self.select_by(-1),
            KeyCode::Char('d') if ctrl => self.scroll_by(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_by(-page / 2),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => self.scroll_by(page),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_by(-page),
            KeyCode::Right | KeyCode::Char('l') => self.pan(8),
            KeyCode::Left | KeyCode::Char('h') => self.pan(-8),
            _ => {}
        }
        Action::None
    }

    pub fn on_mouse(&mut self, m: MouseEvent) -> Action {
        let pos = Position::new(m.column, m.row);
        let over_list = self.has_files() && self.list_area.contains(pos);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if over_list => {
                self.pane = Pane::Files;
                let a = super::render::content(self.list_area);
                if a.contains(pos) {
                    self.select(self.list_offset + (m.row - a.y) as usize);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.body_area.contains(pos) => self.pane = Pane::Body,
            MouseEventKind::ScrollDown if over_list => self.select_by(1),
            MouseEventKind::ScrollUp if over_list => self.select_by(-1),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollRight => self.pan(8),
            MouseEventKind::ScrollLeft => self.pan(-8),
            _ => {}
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, n: usize) -> FileDiff {
        FileDiff {
            path: path.into(),
            status: 'M',
            adds: n,
            dels: 0,
            lines: (0..n).map(|i| format!("+{i}")).collect(),
        }
    }

    fn view() -> DiffView {
        let mut v = DiffView::new(
            1,
            "t".into(),
            diff::Target::Commit {
                dir: "/".into(),
                id: "x".into(),
            },
            None,
        );
        v.body_area = Rect::new(0, 0, 80, 11); // 10 body rows under the title rule
        v.loaded(Ok(DiffDoc {
            overview: vec!["commit x".into()],
            files: vec![file("a", 50), file("b", 3)],
        }));
        v
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn starts_on_first_file_and_navigates() {
        let mut v = view();
        assert_eq!(v.selected_file().unwrap().path, "a");
        v.on_key(key(KeyCode::Char('k')));
        assert_eq!(v.sel, 0, "overview");
        assert_eq!(v.body(), ["commit x"]);
        v.on_key(key(KeyCode::Char('G')));
        assert_eq!(v.selected_file().unwrap().path, "b");
        // Body pane: j scrolls instead of changing file.
        v.on_key(key(KeyCode::Char('p')));
        v.on_key(key(KeyCode::Enter));
        assert_eq!(v.pane, Pane::Body);
        v.on_key(key(KeyCode::Char('j')));
        assert_eq!((v.sel, v.scroll), (1, 1));
        v.on_key(key(KeyCode::Char('G')));
        assert_eq!(v.scroll, 40);
        assert_eq!(v.on_key(key(KeyCode::Esc)), Action::None);
        assert_eq!(v.pane, Pane::Files);
        assert_eq!(v.on_key(key(KeyCode::Esc)), Action::Close);
    }

    #[test]
    fn reload_keeps_file_and_scroll() {
        let mut v = view();
        v.pane = Pane::Body;
        v.scroll_by(7);
        v.begin_reload(2);
        v.loaded(Ok(DiffDoc {
            overview: vec![],
            files: vec![file("new", 1), file("a", 60)],
        }));
        assert_eq!(v.selected_file().unwrap().path, "a");
        assert_eq!(v.scroll, 7);
        // The selected file disappeared: clamp, back to the top.
        v.begin_reload(3);
        v.loaded(Ok(DiffDoc {
            overview: vec!["No uncommitted changes.".into()],
            files: vec![],
        }));
        assert_eq!((v.sel, v.scroll), (0, 0));
        assert_eq!(v.pane, Pane::Body);
    }
}
