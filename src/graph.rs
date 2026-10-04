//! Commit graph layout: assigns each commit a lane and draws the pipes between rows with
//! box-drawing characters, one terminal row per commit (no connector-only rows).
//!
//! Lanes are columns that "expect" a commit id. Walking commits in topological order (children
//! first), a commit takes the leftmost lane expecting it (other lanes expecting it merge in with
//! `╯`), its lane then expects its first parent, and further parents branch out to the right
//! with `╮`. Each lane is two cells wide: the lane glyph and a gap that carries horizontal pipes.
//!
//! Dirty worktrees get a synthetic "uncommitted" row whose only parent is the worktree's HEAD,
//! inserted directly above that commit, so the normal lane logic connects it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::git::log::Commit;
use crate::git::refs::{RefEntry, RefKind};
use crate::model::WorktreeView;

pub const NODE: char = '●';
pub const NODE_MERGE: char = '○';
pub const NODE_HEAD: char = '◉';
pub const NODE_UNCOMMITTED: char = '◌';

/// Color index meaning "no lane color".
pub const NO_COLOR: u8 = u8::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Lane index the cell belongs to (callers map it onto a palette), or [`NO_COLOR`].
    pub color: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// Index into [`Graph::commits`].
    Commit(usize),
    /// Uncommitted changes of the worktree at this index in the snapshot's worktree list.
    Uncommitted(usize),
}

#[derive(Clone, Debug)]
pub struct Row {
    pub kind: RowKind,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LabelKind {
    /// A worktree's HEAD; text is the worktree name (its branch, or directory when detached).
    Worktree,
    Branch,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub kind: LabelKind,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub commits: Arc<Vec<Commit>>,
    pub rows: Vec<Row>,
    /// Labels per commit index.
    pub labels: Vec<Vec<Label>>,
    pub commit_row: HashMap<String, usize>,
    /// Row to jump to for a worktree: its uncommitted row when dirty, else its HEAD commit.
    pub worktree_row: HashMap<PathBuf, usize>,
    /// More history exists beyond the loaded commits.
    pub truncated: bool,
}

const U: u8 = 1;
const D: u8 = 2;
const L: u8 = 4;
const R: u8 = 8;

fn glyph(bits: u8) -> char {
    match bits {
        0 => ' ',
        b if b == U | D || b == U || b == D => '│',
        b if b == L | R || b == L || b == R => '─',
        b if b == U | L => '╯',
        b if b == U | R => '╰',
        b if b == D | L => '╮',
        b if b == D | R => '╭',
        b if b == U | D | L => '┤',
        b if b == U | D | R => '├',
        b if b == U | L | R => '┴',
        b if b == D | L | R => '┬',
        _ => '┼',
    }
}

/// Per-row pipe bits, two slots per lane.
#[derive(Default)]
struct RowCells {
    bits: Vec<u8>,
    colors: Vec<u8>,
    node: Option<(usize, char, u8)>,
}

impl RowCells {
    fn set(&mut self, slot: usize, bits: u8, color: u8) {
        if self.bits.len() <= slot {
            self.bits.resize(slot + 1, 0);
            self.colors.resize(slot + 1, NO_COLOR);
        }
        if self.bits[slot] == 0 {
            self.colors[slot] = color;
        }
        self.bits[slot] |= bits;
    }

    /// Horizontal pipe between lanes `a` and `b` (exclusive of their vertical bits).
    fn hline(&mut self, a: usize, b: usize, color: u8) {
        let (lo, hi) = (a.min(b), a.max(b));
        if lo == hi {
            return;
        }
        self.set(2 * lo, R, color);
        for slot in 2 * lo + 1..2 * hi {
            self.set(slot, L | R, color);
        }
        self.set(2 * hi, L, color);
    }

    fn finish(self) -> Vec<Cell> {
        let mut cells: Vec<Cell> = self
            .bits
            .iter()
            .zip(&self.colors)
            .map(|(&b, &c)| Cell {
                ch: glyph(b),
                color: c,
            })
            .collect();
        if let Some((lane, ch, color)) = self.node {
            let slot = 2 * lane;
            if cells.len() <= slot {
                cells.resize(
                    slot + 1,
                    Cell {
                        ch: ' ',
                        color: NO_COLOR,
                    },
                );
            }
            cells[slot] = Cell { ch, color };
        }
        while cells.last().is_some_and(|c| c.ch == ' ') {
            cells.pop();
        }
        cells
    }
}

fn lane_color(lane: usize) -> u8 {
    (lane % 250) as u8
}

/// One item to lay out: a real commit or a synthetic uncommitted node.
struct Item<'a> {
    id: &'a str,
    parents: &'a [String],
    node: char,
}

/// Lay out `items` (topologically ordered, children first) into rows of cells.
fn layout(items: &[Item]) -> Vec<Vec<Cell>> {
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let mut row = RowCells::default();
        let col = match lanes.iter().position(|l| *l == Some(item.id)) {
            Some(c) => c,
            None => match lanes.iter().position(Option::is_none) {
                Some(c) => c,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };

        // Pipes coming from the row above: continue, or end at this commit.
        let mut freed = HashSet::new();
        for (j, lane) in lanes.iter().enumerate() {
            match lane {
                Some(id) if *id == item.id => {
                    row.set(2 * j, U, lane_color(j));
                    if j != col {
                        row.hline(j, col, lane_color(j));
                        freed.insert(j);
                    }
                }
                Some(_) => row.set(2 * j, U | D, lane_color(j)),
                None => {}
            }
        }
        for &j in &freed {
            lanes[j] = None;
        }
        row.node = Some((col, item.node, lane_color(col)));

        // Pipes going down to the parents.
        match item.parents.split_first() {
            None => lanes[col] = None,
            Some((first, rest)) => {
                lanes[col] = Some(first.as_str());
                row.set(2 * col, D, lane_color(col));
                for p in rest {
                    let existing = lanes
                        .iter()
                        .enumerate()
                        .position(|(k, l)| k != col && *l == Some(p.as_str()));
                    let k = existing.unwrap_or_else(|| {
                        let free = (col + 1..lanes.len())
                            .find(|&k| lanes[k].is_none() && !freed.contains(&k));
                        free.unwrap_or_else(|| {
                            lanes.push(None);
                            lanes.len() - 1
                        })
                    });
                    lanes[k] = Some(p.as_str());
                    row.set(2 * k, D, lane_color(k));
                    row.hline(col, k, lane_color(k));
                }
            }
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(row.finish());
    }
    rows
}

impl Graph {
    pub fn build(
        commits: &Arc<Vec<Commit>>,
        refs: &[RefEntry],
        worktrees: &[WorktreeView],
        truncated: bool,
    ) -> Graph {
        let commit_idx: HashMap<&str, usize> = commits
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id.as_str(), i))
            .collect();

        // Uncommitted rows go directly above their HEAD commit (or on top if it is not loaded).
        let mut above: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut on_top: Vec<usize> = Vec::new();
        for (wi, w) in worktrees.iter().enumerate() {
            if !w.is_dirty() {
                continue;
            }
            match w.head.as_deref().and_then(|h| commit_idx.get(h)) {
                Some(&ci) => above.entry(ci).or_default().push(wi),
                None => on_top.push(wi),
            }
        }

        let heads: HashSet<&str> = worktrees.iter().filter_map(|w| w.head.as_deref()).collect();
        let synthetic_ids: Vec<String> = (0..worktrees.len())
            .map(|wi| format!("\0worktree:{wi}"))
            .collect();
        let head_parent: Vec<Vec<String>> = worktrees
            .iter()
            .map(|w| match &w.head {
                Some(h) if commit_idx.contains_key(h.as_str()) => vec![h.clone()],
                _ => vec![],
            })
            .collect();

        let mut kinds = Vec::with_capacity(commits.len() + worktrees.len());
        kinds.extend(on_top.iter().map(|&wi| RowKind::Uncommitted(wi)));
        for ci in 0..commits.len() {
            if let Some(wis) = above.get(&ci) {
                kinds.extend(wis.iter().map(|&wi| RowKind::Uncommitted(wi)));
            }
            kinds.push(RowKind::Commit(ci));
        }
        let items: Vec<Item> = kinds
            .iter()
            .map(|&kind| match kind {
                RowKind::Uncommitted(wi) => Item {
                    id: &synthetic_ids[wi],
                    parents: &head_parent[wi],
                    node: NODE_UNCOMMITTED,
                },
                RowKind::Commit(ci) => {
                    let c = &commits[ci];
                    let node = if heads.contains(c.id.as_str()) {
                        NODE_HEAD
                    } else if c.parents.len() > 1 {
                        NODE_MERGE
                    } else {
                        NODE
                    };
                    Item {
                        id: &c.id,
                        parents: &c.parents,
                        node,
                    }
                }
            })
            .collect();

        let rows: Vec<Row> = layout(&items)
            .into_iter()
            .zip(kinds)
            .map(|(cells, kind)| Row { kind, cells })
            .collect();

        let mut commit_row = HashMap::new();
        let mut worktree_row = HashMap::new();
        for (ri, row) in rows.iter().enumerate() {
            match row.kind {
                RowKind::Commit(ci) => {
                    commit_row.insert(commits[ci].id.clone(), ri);
                }
                RowKind::Uncommitted(wi) => {
                    worktree_row.insert(worktrees[wi].path.clone(), ri);
                }
            }
        }
        for w in worktrees {
            if let Some(&ri) = w.head.as_ref().and_then(|h| commit_row.get(h)) {
                worktree_row.entry(w.path.clone()).or_insert(ri);
            }
        }

        Graph {
            labels: build_labels(commits, &commit_idx, refs, worktrees),
            commits: commits.clone(),
            rows,
            commit_row,
            worktree_row,
            truncated,
        }
    }
}

/// Worktree labels first; a branch checked out in a worktree is shown only as the worktree label.
fn build_labels(
    commits: &[Commit],
    commit_idx: &HashMap<&str, usize>,
    refs: &[RefEntry],
    worktrees: &[WorktreeView],
) -> Vec<Vec<Label>> {
    let mut labels: Vec<Vec<Label>> = vec![Vec::new(); commits.len()];
    // Prunable worktrees no longer exist on disk; show their branch as a plain branch.
    let worktrees: Vec<&WorktreeView> = worktrees.iter().filter(|w| w.prunable.is_none()).collect();
    let checked_out: HashSet<&str> = worktrees
        .iter()
        .filter_map(|w| w.branch.as_deref())
        .collect();
    for w in worktrees {
        if let Some(&ci) = w.head.as_deref().and_then(|h| commit_idx.get(h)) {
            labels[ci].push(Label {
                kind: LabelKind::Worktree,
                text: w.name.clone(),
            });
        }
    }
    for r in refs {
        let Some(&ci) = commit_idx.get(r.target.as_str()) else {
            continue;
        };
        let kind = match r.kind {
            RefKind::Branch if checked_out.contains(r.name.as_str()) => continue,
            RefKind::Branch => LabelKind::Branch,
            RefKind::Remote => LabelKind::Remote,
            RefKind::Tag => LabelKind::Tag,
        };
        labels[ci].push(Label {
            kind,
            text: r.name.clone(),
        });
    }
    for l in &mut labels {
        l.sort_by_key(|l| l.kind);
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::status::Status;

    fn c(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: id.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: "a".into(),
            time: 0,
            subject: format!("subject {id}"),
        }
    }

    fn render(g: &Graph) -> Vec<String> {
        g.rows
            .iter()
            .map(|r| {
                let cells: String = r.cells.iter().map(|c| c.ch).collect();
                let name = match r.kind {
                    RowKind::Commit(ci) => g.commits[ci].id.clone(),
                    RowKind::Uncommitted(wi) => format!("wt{wi}"),
                };
                format!("{cells} {name}")
            })
            .collect()
    }

    fn build(commits: Vec<Commit>, wts: &[WorktreeView]) -> Graph {
        Graph::build(&Arc::new(commits), &[], wts, false)
    }

    #[test]
    fn linear() {
        let g = build(vec![c("c", &["b"]), c("b", &["a"]), c("a", &[])], &[]);
        assert_eq!(render(&g), ["● c", "● b", "● a"]);
    }

    #[test]
    fn two_branches_from_one_base() {
        let g = build(
            vec![c("d", &["b"]), c("c", &["b"]), c("b", &["a"]), c("a", &[])],
            &[],
        );
        assert_eq!(render(&g), ["● d", "│ ● c", "●─╯ b", "● a"]);
    }

    #[test]
    fn merge_commit() {
        let g = build(vec![c("m", &["a", "f"]), c("f", &["a"]), c("a", &[])], &[]);
        assert_eq!(render(&g), ["○─╮ m", "│ ● f", "●─╯ a"]);
    }

    #[test]
    fn merge_into_existing_lane_and_crossing() {
        // m's second parent (b) is already expected by x's lane, so m joins that lane.
        let g = build(
            vec![
                c("x", &["b"]),
                c("m", &["a", "b"]),
                c("b", &["a"]),
                c("a", &[]),
            ],
            &[],
        );
        assert_eq!(render(&g), ["● x", "├─○ m", "● │ b", "●─╯ a"]);
    }

    #[test]
    fn uncommitted_rows_attach_to_head() {
        let dirty = Status {
            modified: 1,
            ..Default::default()
        };
        let wts = vec![
            WorktreeView {
                path: "/w/main".into(),
                name: "main".into(),
                head: Some("b".into()),
                branch: Some("main".into()),
                status: Some(dirty.clone()),
                ..Default::default()
            },
            WorktreeView {
                path: "/w/feat".into(),
                name: "feat".into(),
                head: Some("c".into()),
                branch: Some("feat".into()),
                status: Some(dirty),
                ..Default::default()
            },
            WorktreeView {
                path: "/w/clean".into(),
                name: "clean".into(),
                head: Some("a".into()),
                status: Some(Status::default()),
                ..Default::default()
            },
        ];
        let g = build(vec![c("c", &["b"]), c("b", &["a"]), c("a", &[])], &wts);
        assert_eq!(render(&g), ["◌ wt1", "◉ c", "│ ◌ wt0", "◉─╯ b", "◉ a"]);
        assert_eq!(g.worktree_row[&PathBuf::from("/w/feat")], 0);
        assert_eq!(g.worktree_row[&PathBuf::from("/w/main")], 2);
        assert_eq!(g.worktree_row[&PathBuf::from("/w/clean")], 4);
        assert_eq!(
            g.labels[0],
            [Label {
                kind: LabelKind::Worktree,
                text: "feat".into()
            }]
        );
    }

    #[test]
    fn labels_dedupe_checked_out_branches() {
        let commits = Arc::new(vec![c("b", &["a"]), c("a", &[])]);
        let refs = vec![
            RefEntry {
                kind: RefKind::Tag,
                name: "v1".into(),
                target: "a".into(),
            },
            RefEntry {
                kind: RefKind::Branch,
                name: "main".into(),
                target: "b".into(),
            },
            RefEntry {
                kind: RefKind::Branch,
                name: "old".into(),
                target: "a".into(),
            },
            RefEntry {
                kind: RefKind::Remote,
                name: "origin/main".into(),
                target: "b".into(),
            },
        ];
        let wts = vec![WorktreeView {
            name: "main".into(),
            head: Some("b".into()),
            branch: Some("main".into()),
            ..Default::default()
        }];
        let g = Graph::build(&commits, &refs, &wts, false);
        let texts = |i: usize| {
            g.labels[i]
                .iter()
                .map(|l| (l.kind, l.text.as_str()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            texts(0),
            [
                (LabelKind::Worktree, "main"),
                (LabelKind::Remote, "origin/main")
            ]
        );
        assert_eq!(
            texts(1),
            [(LabelKind::Branch, "old"), (LabelKind::Tag, "v1")]
        );
    }

    #[test]
    fn missing_parent_keeps_lane_open() {
        let g = build(vec![c("b", &["zz"]), c("a", &["yy"])], &[]);
        assert_eq!(render(&g), ["● b", "│ ● a"]);
    }
}
