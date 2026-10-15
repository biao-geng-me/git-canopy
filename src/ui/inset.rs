//! Inline ("inset") expansion inside the overview lists: a commit or worktree row expands into
//! its diff summary (commit message body + changed files), and a file expands into its diff.
//!
//! Each panel shows a flat list of [`VLine`]s: every base row, followed by the inset lines of the
//! rows that are open. The cursor moves over these lines, so arrows step through insets too.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::diff::{DiffDoc, FileDiff};
use crate::git::status::Status;
use crate::model::Host;

/// Identity of something that can be expanded or diffed: a commit, or a worktree's uncommitted
/// changes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    Commit(String),
    /// A worktree, by its path on its host.
    Worktree(Host, PathBuf),
    /// A repository's header row in the worktree list (by common dir); never expands.
    Repo(Host, PathBuf),
}

/// One inset line under base row `row`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Loading,
    Error,
    /// Nothing changed (e.g. a clean worktree that was dirty when expanded).
    Empty,
    /// Continuation line `i` (≥ 1) of a wrapped commit subject.
    Subject(usize),
    /// Line `i` of the (wrapped) commit message body.
    Message(usize),
    /// File `i` of the doc.
    File(usize),
    /// Line `line` of file `file`.
    Diff(usize, usize),
    /// `n` more lines of file `file` not shown inline.
    More(usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VLine {
    Row(usize),
    Inset(usize, Item),
}

impl VLine {
    pub fn row(self) -> usize {
        match self {
            VLine::Row(r) | VLine::Inset(r, _) => r,
        }
    }
}

/// Diff lines shown inline per file before "… n more lines".
pub const MAX_INLINE: usize = 200;

/// Which rows (by row key) are open, and which of their files.
#[derive(Clone, Debug, Default)]
pub struct Expansion {
    open: HashMap<Key, HashSet<String>>,
}

impl Expansion {
    pub fn is_open(&self, row: &Key) -> bool {
        self.open.contains_key(row)
    }

    pub fn keys(&self) -> impl Iterator<Item = &Key> {
        self.open.keys()
    }

    /// Returns whether the row is open afterwards.
    pub fn toggle(&mut self, row: &Key) -> bool {
        if self.open.remove(row).is_some() {
            false
        } else {
            self.open.insert(row.clone(), HashSet::new());
            true
        }
    }

    pub fn open(&mut self, row: &Key) {
        self.open.entry(row.clone()).or_default();
    }

    pub fn close(&mut self, row: &Key) {
        self.open.remove(row);
    }

    pub fn file_open(&self, row: &Key, path: &str) -> bool {
        self.open.get(row).is_some_and(|f| f.contains(path))
    }

    pub fn toggle_file(&mut self, row: &Key, path: &str) {
        if let Some(files) = self.open.get_mut(row)
            && !files.remove(path)
        {
            files.insert(path.to_string());
        }
    }

    pub fn close_file(&mut self, row: &Key, path: &str) {
        if let Some(files) = self.open.get_mut(row) {
            files.remove(path);
        }
    }
}

/// A loaded (or loading) diff for a [`Key`].
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub doc: Option<Result<Arc<DiffDoc>, String>>,
    /// Commit message body lines (after the subject).
    pub message: Vec<String>,
    /// For worktrees: the status the doc was loaded for; a different one means it is stale.
    pub stamp: Option<(Option<Status>, Option<i64>)>,
    /// Id of the request in flight (responses with other ids are stale).
    pub req: u64,
}

impl Entry {
    pub fn set(&mut self, result: Result<DiffDoc, String>) {
        self.message = result.as_ref().map(message_body).unwrap_or_default();
        self.doc = Some(result.map(Arc::new));
    }

    pub fn ok(&self) -> Option<&DiffDoc> {
        match &self.doc {
            Some(Ok(d)) => Some(d),
            _ => None,
        }
    }
}

/// The commit message body from `git show --format=fuller` output: the indented lines after
/// the subject, up to the `---` before the diffstat. Empty for worktree diffs.
pub fn message_body(doc: &DiffDoc) -> Vec<String> {
    if !doc.overview.first().is_some_and(|l| l.starts_with("commit ")) {
        return Vec::new();
    }
    let mut lines = doc
        .overview
        .iter()
        .skip_while(|l| !l.starts_with("    "))
        .skip(1) // subject
        .take_while(|l| l.starts_with("    ") || l.is_empty())
        .map(|l| l.strip_prefix("    ").unwrap_or(l).to_string())
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// Index of the first line worth showing inline: the first hunk, skipping the file header.
pub fn diff_start(file: &FileDiff) -> usize {
    file.lines
        .iter()
        .position(|l| l.starts_with("@@"))
        .unwrap_or(1.min(file.lines.len()))
}

/// An open row of a panel.
#[derive(Clone, Debug)]
pub struct Open {
    pub row: usize,
    pub row_key: Key,
    pub target: Key,
    /// Lines the wrapped subject takes (the first is on the row itself).
    pub subject_lines: usize,
    /// Lines the wrapped message body takes; `None` = one per message line.
    pub message_lines: Option<usize>,
}

/// The visual lines of a panel with `n` base rows and the given open rows.
pub fn build(n: usize, opened: &[Open], exp: &Expansion, docs: &HashMap<Key, Entry>) -> Vec<VLine> {
    let insets: BTreeMap<usize, &Open> = opened.iter().map(|o| (o.row, o)).collect();
    let mut out = Vec::with_capacity(n + insets.len() * 8);
    let mut next = 0;
    for (&r, &open) in &insets {
        if r >= n {
            break;
        }
        out.extend((next..=r).map(VLine::Row));
        next = r + 1;
        inset_lines(open, docs.get(&open.target), exp, &mut out);
    }
    out.extend((next..n).map(VLine::Row));
    out
}

fn inset_lines(open: &Open, entry: Option<&Entry>, exp: &Expansion, out: &mut Vec<VLine>) {
    let (r, rk) = (open.row, &open.row_key);
    let item = |i| VLine::Inset(r, i);
    out.extend((1..open.subject_lines).map(|i| item(Item::Subject(i))));
    let Some(entry) = entry else {
        out.push(item(Item::Loading));
        return;
    };
    let doc = match &entry.doc {
        None => return out.push(item(Item::Loading)),
        Some(Err(_)) => return out.push(item(Item::Error)),
        Some(Ok(doc)) => doc,
    };
    let message_lines = open.message_lines.unwrap_or(entry.message.len());
    out.extend((0..message_lines).map(|i| item(Item::Message(i))));
    if doc.files.is_empty() && message_lines == 0 {
        out.push(item(Item::Empty));
    }
    for (fi, f) in doc.files.iter().enumerate() {
        out.push(item(Item::File(fi)));
        if exp.file_open(rk, &f.path) {
            let start = diff_start(f);
            let end = f.lines.len().min(start + MAX_INLINE);
            out.extend((start..end).map(|li| item(Item::Diff(fi, li))));
            if f.lines.len() > end {
                out.push(item(Item::More(fi, f.lines.len() - end)));
            }
        }
    }
}

/// Word-wrap `text`: the first line gets `first` columns, the rest `rest`. Words longer than a
/// line are split.
pub fn wrap(text: &str, first: usize, rest: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let mut lines = vec![String::new()];
    let limit = |n: usize| if n == 1 { first.max(1) } else { rest.max(1) };
    for word in text.split(' ') {
        let max = limit(lines.len());
        let cur = lines.last_mut().unwrap();
        let sep = usize::from(!cur.is_empty());
        if cur.width() + sep + word.width() <= max {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            continue;
        }
        if !cur.is_empty() {
            lines.push(String::new());
        }
        for c in word.chars() {
            let n = lines.len();
            let cur = lines.last_mut().unwrap();
            if cur.width() + c.width().unwrap_or(0) > limit(n) {
                lines.push(String::new());
            }
            lines.last_mut().unwrap().push(c);
        }
    }
    lines
}

/// Position of base row `r` in `lines` (lines are ordered by row).
pub fn index_of_row(lines: &[VLine], r: usize) -> Option<usize> {
    lines
        .binary_search_by(|l| {
            l.row().cmp(&r).then(if matches!(l, VLine::Row(_)) {
                std::cmp::Ordering::Equal
            } else {
                std::cmp::Ordering::Greater
            })
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, n: usize) -> FileDiff {
        let mut lines = vec![format!("diff --git a/{path} b/{path}"), "@@ -1 +1 @@".into()];
        lines.extend((0..n).map(|i| format!("+{i}")));
        FileDiff {
            path: path.into(),
            status: 'M',
            adds: n,
            dels: 0,
            lines,
            cells: Vec::new(),
        }
    }

    fn entry(files: Vec<FileDiff>) -> Entry {
        let mut e = Entry::default();
        e.set(Ok(DiffDoc {
            columns: Vec::new(),
            overview: [
                "commit x",
                "Author: a",
                "",
                "    subject",
                "    ",
                "    body line",
                "    ",
                "---",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            files,
        }));
        e
    }

    #[test]
    fn message_body_skips_subject() {
        assert_eq!(entry(vec![]).message, ["body line"]);
        let worktree = DiffDoc {
            columns: Vec::new(),
            overview: vec!["Untracked files (1):".into(), "    notes.md".into()],
            files: vec![],
        };
        assert!(message_body(&worktree).is_empty());
    }

    #[test]
    fn builds_rows_and_insets() {
        let k = Key::Commit("x".into());
        let mut exp = Expansion::default();
        exp.toggle(&k);
        exp.toggle_file(&k, "b");
        let mut docs = HashMap::new();
        docs.insert(k.clone(), entry(vec![file("a", 1), file("b", MAX_INLINE + 5)]));
        let open = |row: usize, target: Key| Open {
            row,
            row_key: k.clone(),
            target,
            subject_lines: 1,
            message_lines: None,
        };
        let lines = build(3, &[open(1, k.clone())], &exp, &docs);
        assert_eq!(lines[0], VLine::Row(0));
        assert_eq!(lines[1], VLine::Row(1));
        assert_eq!(lines[2], VLine::Inset(1, Item::Message(0)));
        assert_eq!(lines[3], VLine::Inset(1, Item::File(0)));
        assert_eq!(lines[4], VLine::Inset(1, Item::File(1)));
        assert_eq!(lines[5], VLine::Inset(1, Item::Diff(1, 1)), "starts at the hunk header");
        assert_eq!(lines[5 + MAX_INLINE], VLine::Inset(1, Item::More(1, 6)));
        assert_eq!(*lines.last().unwrap(), VLine::Row(2));
        assert_eq!(index_of_row(&lines, 2), Some(lines.len() - 1));
        assert_eq!(index_of_row(&lines, 1), Some(1));

        // Not loaded yet.
        let lines = build(2, &[open(0, Key::Commit("other".into()))], &exp, &docs);
        assert_eq!(lines, [VLine::Row(0), VLine::Inset(0, Item::Loading), VLine::Row(1)]);

        // A wrapped subject puts its continuation lines first; wrapped message line counts win.
        let o = Open {
            subject_lines: 3,
            message_lines: Some(2),
            ..open(0, k.clone())
        };
        let lines = build(1, &[o], &Expansion::default(), &docs);
        assert_eq!(
            lines[..5],
            [
                VLine::Row(0),
                VLine::Inset(0, Item::Subject(1)),
                VLine::Inset(0, Item::Subject(2)),
                VLine::Inset(0, Item::Message(0)),
                VLine::Inset(0, Item::Message(1)),
            ]
        );
    }

    #[test]
    fn wraps_words() {
        assert_eq!(wrap("fix the parser for eof", 10, 20), ["fix the", "parser for eof"]);
        assert_eq!(wrap("short", 10, 10), ["short"]);
        assert_eq!(wrap("abcdefghij", 4, 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 5, 5), [""]);
    }
}
