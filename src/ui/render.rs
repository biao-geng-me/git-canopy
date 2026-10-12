//! Drawing. Pure functions of [`App`] state (plus scroll-offset bookkeeping).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::copy;
use super::diff_view::Pane;
use super::inset::{self, Item, VLine};
use super::{App, Focus, InsetText, LayoutMode, WtFields};
use crate::diff::FileDiff;
use crate::graph::{self, LabelKind, Row, RowKind};
use crate::herdr::{self, AgentState};
use crate::model::{Cleanup, Landed, WorktreeView};

pub const C_STAGED: Color = Color::Green;
pub const C_MODIFIED: Color = Color::Yellow;
pub const C_UNTRACKED: Color = Color::Magenta;
pub const C_CONFLICT: Color = Color::Red;
pub const C_DIM: Color = Color::DarkGray;
/// Remote branch labels: red, which no lane uses, so they never look like local branches (a gray
/// blended in with the dim age and parentheses around it).
const C_REMOTE: Color = Color::Red;
/// Commit hashes: a muted yellow (dark goldenrod), present but quieter than the subject.
const C_HASH: Color = Color::Indexed(136);
/// Overlapping changes between worktrees: a likely conflict, softer than the red of real ones.
pub const C_OVERLAP: Color = Color::LightRed;
const SEL_FOCUSED: Color = Color::Indexed(238);
const SEL_UNFOCUSED: Color = Color::Indexed(235);

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, rule, footer] =
        Layout::vertical([Constraint::Min(2), Constraint::Length(1), Constraint::Length(2)]).areas(f.area());
    // The footer gets the same plain rule as the panes above it.
    f.render_widget(
        Block::new().borders(Borders::TOP).border_style(border_style(false)),
        rule,
    );
    if app.diff.is_some() {
        draw_diff(f, app, main);
        draw_diff_footer(f, app, footer);
        if app.help {
            draw_help(f);
        }
        return;
    }
    // Stacked: graph on top, worktrees below, both full width. Narrow terminals always stack.
    let stacked = app.layout == LayoutMode::Stacked || main.width < NARROW;
    let (wt_area, graph_area) = if stacked {
        let want = app.wt.lines.len().max(1) as u16 + CHROME;
        let h = want.min(main.height / 2).max(2.min(main.height));
        let [top, bottom] = Layout::vertical([Constraint::Min(0), Constraint::Length(h)]).areas(main);
        (bottom, top)
    } else {
        let wt_width = worktree_panel_width(app, main.width);
        let [left, right] = Layout::horizontal([Constraint::Length(wt_width), Constraint::Min(10)]).areas(main);
        (left, right)
    };
    // An inset shell takes the lower half of the graph's space.
    let (graph_area, shell_area) = if app.shell.is_some() {
        let [top, bottom] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(graph_area);
        (top, bottom)
    } else {
        (graph_area, Rect::default())
    };
    app.areas.worktrees = wt_area;
    app.areas.graph = graph_area;
    app.areas.shell = shell_area;
    draw_worktrees(f, app, wt_area, stacked);
    draw_graph(f, app, graph_area);
    draw_shell(f, app, shell_area);
    draw_footer(f, app, footer);
    if let Some(plan) = &app.confirm {
        draw_confirm(f, plan);
    }
    if app.help {
        draw_help(f);
    }
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(C_DIM)
    }
}

/// A pane: a horizontal rule on top carrying the title, no side lines (they add clutter inside
/// tmux/herdr, which draw their own); one blank column on each side instead. See [`content`].
fn panel(title: Line<'static>, focused: bool) -> Block<'static> {
    // A short lead-in of rule before the title: `── graph ────…`.
    let mut spans = vec![Span::styled("──", border_style(focused))];
    spans.extend(title.spans);
    Block::new()
        .borders(Borders::TOP)
        .border_style(border_style(focused))
        .padding(Padding::horizontal(1))
        .title(Line::from(spans))
}

/// Rows a [`panel`] uses for its rule.
pub const CHROME: u16 = 1;

/// The content area of a [`panel`] drawn in `area` (for hit-testing and paging).
pub fn content(area: Rect) -> Rect {
    panel(Line::default(), false).inner(area)
}

/// Paint `lines` into `inner`, one per row, highlighting row `sel` across the full width.
fn paint_rows(f: &mut Frame, inner: Rect, lines: Vec<Line<'static>>, sel: Option<usize>, focused: bool) {
    for (i, line) in lines.into_iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let row = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let mut p = Paragraph::new(line);
        if sel == Some(i) {
            p = p.style(Style::new().bg(if focused { SEL_FOCUSED } else { SEL_UNFOCUSED }));
        }
        f.render_widget(p, row);
    }
}

// ---------------------------------------------------------------------------------------------
// Worktree panel

/// Pieces of one worktree row, before alignment.
struct WtRow {
    marker: Span<'static>,
    name: Span<'static>,
    /// herdr agent marker (empty when no agent works here).
    agent: Vec<Span<'static>>,
    counts: Vec<Span<'static>>,
    ab: Vec<Span<'static>>,
    /// `+412 -38` since forking from the base branch (wide rows only).
    size: Vec<Span<'static>>,
    /// `⚠ feat-x +1` (wide) or `⚠2`: worktrees that changed the same files.
    overlap: Vec<Span<'static>>,
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

pub fn marker(w: &WorktreeView) -> Span<'static> {
    let st = w.status.as_ref();
    if w.prunable.is_some() {
        Span::styled("✗", Style::new().fg(C_DIM))
    } else if w.bare {
        Span::styled("◇", Style::new().fg(C_DIM))
    } else if w.status_error.is_some() && st.is_none() {
        Span::styled("!", Style::new().fg(C_CONFLICT))
    } else if st.is_some_and(|s| s.unmerged > 0) || w.op.is_some() {
        Span::styled("⚠", Style::new().fg(C_CONFLICT))
    } else if w.is_dirty() {
        Span::styled("✎", Style::new().fg(C_MODIFIED))
    } else if st.is_some() {
        Span::styled("●", Style::new().fg(C_STAGED))
    } else {
        Span::styled("…", Style::new().fg(C_DIM))
    }
}

const CLEANUP_GLYPH: &str = "⌫";

/// `⌫` when a worktree could be cleaned up: green when nothing would be lost, yellow when its
/// untracked files would be.
pub fn cleanup_span(c: Cleanup) -> Option<Span<'static>> {
    match c {
        Cleanup::Safe => Some(Span::styled(CLEANUP_GLYPH, Style::new().fg(Color::Green))),
        Cleanup::Untracked => Some(Span::styled(CLEANUP_GLYPH, Style::new().fg(C_MODIFIED))),
        Cleanup::No => None,
    }
}

/// Spaces after the worktree rows' marker column: two when a row shows `⌫`, which many fonts
/// draw wider than its cell.
fn marker_gap(rows: &[WtRow]) -> usize {
    if rows.iter().any(|r| r.marker.content == CLEANUP_GLYPH) { 2 } else { 1 }
}

/// `+staged ~modified ?untracked !unmerged`, or a state word when there are no counts.
pub fn count_spans(w: &WorktreeView) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    if let Some(st) = &w.status {
        for (n, sym, color) in [
            (st.staged, '+', C_STAGED),
            (st.modified, '~', C_MODIFIED),
            (st.untracked, '?', C_UNTRACKED),
            (st.unmerged, '!', C_CONFLICT),
        ] {
            if n > 0 {
                if !out.is_empty() {
                    out.push(Span::raw(" "));
                }
                out.push(Span::styled(format!("{sym}{n}"), Style::new().fg(color)));
            }
        }
    }
    if out.is_empty() {
        let word = if w.prunable.is_some() {
            "prunable"
        } else if w.bare {
            "bare"
        } else if w.status.is_some() {
            "clean"
        } else if w.status_error.is_some() {
            "error"
        } else {
            ""
        };
        out.push(Span::styled(word, Style::new().fg(C_DIM)));
    }
    out
}

/// Ahead/behind: vs the base branch (`↑a ↓b`, or `merged` once its work is in base), and vs
/// upstream (`⇡a ⇣b`) when not in sync.
fn ab_spans(w: &WorktreeView, base: Option<&str>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let is_base = base.is_some() && w.branch.as_deref() == base;
    if w.is_merged() {
        out.push(Span::styled("merged", Style::new().fg(Color::Green)));
    } else if !is_base && let Some((a, b)) = w.vs_base {
        if a > 0 {
            out.push(Span::styled(format!("↑{a}"), Style::new().fg(Color::Green)));
        }
        if b > 0 {
            out.push(Span::styled(format!("↓{b}"), Style::new().fg(Color::Red)));
        }
    }
    if let Some((a, b)) = w.status.as_ref().and_then(|s| s.upstream_ab)
        && (a, b) != (0, 0)
    {
        let mut s = String::new();
        if a > 0 {
            s.push_str(&format!("⇡{a}"));
        }
        if b > 0 {
            s.push_str(&format!("⇣{b}"));
        }
        out.push(Span::styled(s, Style::new().fg(Color::Blue)));
    }
    if let Some(op) = w.op {
        out.push(Span::styled(
            op,
            Style::new().fg(C_CONFLICT).add_modifier(Modifier::BOLD),
        ));
    }
    let mut spaced = Vec::new();
    for (i, s) in out.into_iter().enumerate() {
        if i > 0 {
            spaced.push(Span::raw(" "));
        }
        spaced.push(s);
    }
    spaced
}

/// What a worktree changed since forking from the base branch: `+412 -38` (empty when nothing,
/// or when that is in base already).
fn size_spans(w: &WorktreeView) -> Vec<Span<'static>> {
    let Some(ch) = w.changes.as_ref().filter(|_| !w.is_merged()) else {
        return Vec::new();
    };
    let k = |n: u64| {
        if n >= 10_000 {
            format!("{}k", n / 1000)
        } else {
            n.to_string()
        }
    };
    let mut out = Vec::new();
    if ch.adds > 0 {
        out.push(Span::styled(format!("+{}", k(ch.adds)), Style::new().fg(Color::Green)));
    }
    if ch.dels > 0 {
        if !out.is_empty() {
            out.push(Span::raw(" "));
        }
        out.push(Span::styled(format!("-{}", k(ch.dels)), Style::new().fg(Color::Red)));
    }
    out
}

/// Other worktrees that changed the same files: `⚠ feat-x +1` when `wide`, else `⚠2`.
fn overlap_spans(w: &WorktreeView, wide: bool) -> Vec<Span<'static>> {
    let Some(first) = w.overlaps.first() else {
        return Vec::new();
    };
    let style = Style::new().fg(C_OVERLAP);
    let n = w.overlaps.len();
    if !wide {
        return vec![Span::styled(format!("⚠{n}"), style)];
    }
    let mut out = vec![Span::styled(format!("⚠ {}", truncate(&first.with, 20)), style)];
    if n > 1 {
        out.push(Span::styled(format!(" +{}", n - 1), Style::new().fg(C_DIM)));
    }
    out
}

/// Marker for the agents in a worktree: `◆ blocked`, `▶ working ×2`, … (glyph only when not
/// `wide`). Blocked is bold red: the agent needs the user.
pub fn agent_spans(act: herdr::Activity, wide: bool) -> Vec<Span<'static>> {
    let (glyph, style) = match act.state {
        AgentState::Blocked => ("◆", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
        AgentState::Working => ("▶", Style::new().fg(Color::LightCyan)),
        AgentState::Done => ("✓", Style::new().fg(Color::Green)),
        AgentState::Idle => ("·", Style::new().fg(C_DIM)),
        AgentState::Unknown => ("?", Style::new().fg(C_DIM)),
    };
    let mut text = glyph.to_string();
    if wide {
        text.push(' ');
        text.push_str(act.state.word());
    }
    let mut spans = vec![Span::styled(text, style)];
    if act.count > 1 {
        spans.push(Span::styled(format!(" ×{}", act.count), Style::new().fg(C_DIM)));
    }
    spans
}

fn wt_rows(app: &App, wide: bool) -> Vec<WtRow> {
    let base = app.snap.base.as_deref();
    app.snap
        .worktrees
        .iter()
        .enumerate()
        .map(|(wi, w)| {
            // Named in the same color as its lane in the graph.
            let mut style = worktree_color(app, w).map_or(Style::new(), lane_style);
            if w.is_current {
                style = style.add_modifier(Modifier::BOLD);
            }
            if w.prunable.is_some() || w.bare {
                style = style.fg(C_DIM);
            }
            WtRow {
                marker: cleanup_span(app.cleanup(wi)).unwrap_or_else(|| marker(w)),
                name: Span::styled(w.name.clone(), style),
                agent: app.agent_activity(wi).map(|a| agent_spans(a, wide)).unwrap_or_default(),
                counts: count_spans(w),
                ab: ab_spans(w, base),
                size: if wide { size_spans(w) } else { Vec::new() },
                overlap: overlap_spans(w, wide),
            }
        })
        .collect()
}

const NAME_MAX: usize = 28;
/// Below this width the panels are stacked vertically.
const NARROW: u16 = 72;

fn worktree_panel_width(app: &App, total: u16) -> u16 {
    let rows = wt_rows(app, false);
    let name = rows
        .iter()
        .map(|r| r.name.content.width())
        .max()
        .unwrap_or(4)
        .min(NAME_MAX);
    let counts = rows.iter().map(|r| spans_width(&r.counts)).max().unwrap_or(0);
    let ab = rows.iter().map(|r| spans_width(&r.ab)).max().unwrap_or(0);
    let agent = rows.iter().map(|r| spans_width(&r.agent)).max().unwrap_or(0);
    let overlap = rows.iter().map(|r| spans_width(&r.overlap)).max().unwrap_or(0);
    // side margins + marker + gaps
    let natural = 2
        + 1
        + marker_gap(&rows)
        + name
        + 1
        + if agent > 0 { agent + 1 } else { 0 }
        + counts
        + if ab > 0 { 1 + ab } else { 0 }
        + if overlap > 0 { 1 + overlap } else { 0 }
        + 1;
    let max = total as usize * 45 / 100;
    natural.max(24).min(max) as u16
}

fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn pad(s: String, width: usize) -> String {
    let w = s.width();
    if w >= width { s } else { s + &" ".repeat(width - w) }
}

/// Column widths shared by all worktree rows.
struct WtCols {
    /// Spaces between the marker and the name.
    marker_gap: usize,
    name: usize,
    /// herdr agent marker column (0 when no worktree has an agent).
    agent: usize,
    counts: usize,
    ab: usize,
    /// Change size column (wide rows only).
    size: usize,
    /// Overlap marker column (0 when no worktree overlaps another).
    overlap: usize,
    /// "edited 3m" column (wide rows only).
    activity: usize,
    /// Path column (wide rows only).
    path: usize,
}

fn activity_text(w: &WorktreeView, now: i64) -> Option<String> {
    w.is_dirty().then_some(())?;
    Some(format!("edited {}", age(w.changed_at?, now)))
}

/// Keep the end of a path that does not fit: `…/wt/feat-auth`.
fn truncate_left(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out: Vec<char> = Vec::new();
    let mut w = 1;
    for c in s.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out.into_iter().rev().collect()
}

/// Deepest directory containing all `paths` (when there are at least two).
fn common_parent<'a>(paths: impl Iterator<Item = &'a std::path::Path>) -> Option<std::path::PathBuf> {
    let mut paths = paths.peekable();
    let first = paths.next()?;
    paths.peek()?;
    let mut common: std::path::PathBuf = first.parent()?.to_path_buf();
    for p in paths {
        while !p.starts_with(&common) {
            common = common.parent()?.to_path_buf();
        }
    }
    Some(common)
}

/// The columns `field` will take when appended after `before` (`None` when it's empty).
fn field_range(before: &[Span], field: &[Span]) -> Option<std::ops::Range<u16>> {
    let (at, w) = (spans_width(before) as u16, spans_width(field) as u16);
    (w > 0).then_some(at..at + w)
}

#[allow(clippy::too_many_arguments)]
fn wt_line(
    row: WtRow,
    w: &WorktreeView,
    shown_path: &str,
    cols: &WtCols,
    wide: bool,
    width: usize,
    now: i64,
) -> (Line<'static>, WtFields) {
    let dim = Style::new().fg(C_DIM);
    let mut fields = WtFields::default();
    let mut spans = vec![row.marker, Span::raw(" ".repeat(cols.marker_gap))];
    spans.push(Span::styled(
        pad(truncate(&row.name.content, cols.name), cols.name),
        row.name.style,
    ));
    spans.push(Span::raw(" "));
    if cols.agent > 0 {
        let aw = spans_width(&row.agent);
        fields.agent = field_range(&spans, &row.agent);
        spans.extend(row.agent);
        spans.push(Span::raw(" ".repeat(cols.agent - aw + 1)));
    }
    let cw = spans_width(&row.counts);
    spans.extend(row.counts);
    if cols.ab > 0 || cols.overlap > 0 || wide {
        spans.push(Span::raw(" ".repeat(cols.counts - cw + 1)));
        let aw = spans_width(&row.ab);
        spans.extend(row.ab);
        if wide {
            spans.push(Span::raw(" ".repeat(cols.ab - aw + 2)));
        } else if cols.overlap > 0 {
            spans.push(Span::raw(" ".repeat(cols.ab - aw + usize::from(cols.ab > 0))));
        }
    }
    if !wide {
        fields.overlap = field_range(&spans, &row.overlap);
        spans.extend(row.overlap);
        return (Line::from(spans), fields);
    }
    if cols.size > 0 {
        let sw = spans_width(&row.size);
        spans.extend(row.size);
        spans.push(Span::raw(" ".repeat(cols.size - sw + 2)));
    }
    if cols.overlap > 0 {
        let ow = spans_width(&row.overlap);
        fields.overlap = field_range(&spans, &row.overlap);
        spans.extend(row.overlap);
        spans.push(Span::raw(" ".repeat(cols.overlap - ow + 2)));
    }

    // Wide rows: last edit, HEAD commit (sha, age), path on the right.
    let activity = activity_text(w, now).unwrap_or_default();
    if cols.activity > 0 {
        spans.push(Span::styled(pad(activity, cols.activity), Style::new().fg(C_MODIFIED)));
        spans.push(Span::raw("  "));
    }
    let used = spans_width(&spans);
    let path = truncate_left(shown_path, cols.path);
    let room = width.saturating_sub(used + cols.path + 2);
    // HEAD commit: sha and age (its subject is in the graph).
    if let Some(h) = &w.head
        && room > 12
    {
        spans.push(Span::styled(h[..h.len().min(7)].to_string(), Style::new().fg(C_HASH)));
        let head_age = w.head_time.map(|t| age(t, now)).unwrap_or_default();
        spans.push(Span::styled(format!(" {head_age:>3}"), dim));
    }
    let used = spans_width(&spans);
    if cols.path > 0 && used + 2 + path.width() <= width {
        spans.push(Span::raw(" ".repeat(width - used - path.width())));
        fields.path = Some((width - path.width()) as u16..width as u16);
        spans.push(Span::styled(path, dim));
    }
    (Line::from(spans), fields)
}

/// An inset line's content (without indentation): message, file entry or diff line.
fn inset_spans(app: &App, f: Focus, r: usize, item: Item) -> Vec<Span<'static>> {
    let dim = Style::new().fg(C_DIM);
    let entry = app.row_doc(f, r);
    let file = |fi: usize| entry.and_then(|e| e.ok()).and_then(|d| d.files.get(fi));
    match item {
        Item::Loading => vec![Span::styled("loading…", dim)],
        Item::Error => {
            let msg = match entry.and_then(|e| e.doc.as_ref()) {
                Some(Err(e)) => e.lines().next().unwrap_or("").to_string(),
                _ => String::new(),
            };
            vec![Span::styled(msg, Style::new().fg(C_CONFLICT))]
        }
        Item::Empty => vec![Span::styled("no changes", dim)],
        Item::Subject(_) => vec![],
        Item::Message(i) => {
            let text = entry.and_then(|e| e.message.get(i)).cloned().unwrap_or_default();
            vec![Span::styled(text, dim.add_modifier(Modifier::ITALIC))]
        }
        Item::File(fi) => {
            let Some(file) = file(fi) else { return vec![] };
            let open = app
                .row_key(f, r)
                .is_some_and(|k| app.panel(f).exp.file_open(&k, &file.path));
            let mut spans = vec![
                Span::styled(if open { "▾ " } else { "▸ " }, dim),
                Span::styled(format!("{} ", file.status), Style::new().fg(status_color(file.status))),
                Span::raw(file.path.clone()),
                Span::raw("  "),
            ];
            spans.extend(add_del_spans(file.adds, file.dels));
            spans
        }
        Item::Diff(fi, li) => {
            let Some(line) = file(fi).and_then(|f| f.lines.get(li)) else {
                return vec![];
            };
            vec![Span::raw("  "), Span::styled(line.clone(), diff_line_style(line))]
        }
        Item::More(_, n) => vec![Span::styled(
            format!("  … {n} more lines · Enter for the full diff"),
            dim,
        )],
    }
}

fn draw_worktrees(f: &mut Frame, app: &mut App, area: Rect, wide: bool) {
    let focused = app.focus == Focus::Worktrees && !app.shell_focus;
    let n = app.snap.worktrees.len();
    let dirty = app.snap.worktrees.iter().filter(|w| w.is_dirty()).count();
    let mut title = vec![Span::raw(" worktrees ")];
    if n > 0 {
        title.push(Span::styled(format!("{n} "), Style::new().fg(C_DIM)));
        if dirty > 0 {
            title.push(Span::styled(format!("✎ {dirty} "), Style::new().fg(C_MODIFIED)));
        }
    }
    let block = panel(Line::from(title), focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if n == 0 {
        f.render_widget(Paragraph::new(Span::styled(" loading…", Style::new().fg(C_DIM))), inner);
        return;
    }

    let now = now_secs();
    let rows = wt_rows(app, wide);
    let width = inner.width as usize;
    let counts = rows.iter().map(|r| spans_width(&r.counts)).max().unwrap_or(0);
    let ab = rows.iter().map(|r| spans_width(&r.ab)).max().unwrap_or(0);
    let name_natural = rows.iter().map(|r| r.name.content.width()).max().unwrap_or(0);
    let size = rows.iter().map(|r| spans_width(&r.size)).max().unwrap_or(0);
    let overlap = rows.iter().map(|r| spans_width(&r.overlap)).max().unwrap_or(0);
    let gap = marker_gap(&rows);
    let fixed = 1
        + gap
        + 1
        + counts
        + if ab > 0 { 1 + ab } else { 0 }
        + if size > 0 { 2 + size } else { 0 }
        + if overlap > 0 { 2 + overlap } else { 0 };
    let activity = if wide {
        app.snap
            .worktrees
            .iter()
            .filter_map(|w| activity_text(w, now))
            .map(|s| s.width())
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let base = common_parent(app.snap.worktrees.iter().map(|w| w.path.as_path()));
    let shown_path = |w: &WorktreeView| match &base {
        Some(b) => w
            .path
            .strip_prefix(b)
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        None => tilde(&w.path),
    };
    let path = if wide {
        let longest = app
            .snap
            .worktrees
            .iter()
            .map(|w| shown_path(w).width())
            .max()
            .unwrap_or(0);
        longest.min(width / 4)
    } else {
        0
    };
    let agent = rows.iter().map(|r| spans_width(&r.agent)).max().unwrap_or(0);
    let cols = WtCols {
        marker_gap: gap,
        name: name_natural
            .min(NAME_MAX)
            .min(width.saturating_sub(fixed + agent).max(6)),
        agent,
        counts,
        ab,
        size,
        overlap,
        activity,
        path,
    };

    let height = inner.height as usize;
    app.wt.pos.clamp(app.wt.lines.len());
    app.wt.pos.scroll_into_view(height);
    let mut rows: Vec<Option<WtRow>> = rows.into_iter().map(Some).collect();
    let visible: Vec<VLine> = app
        .wt
        .lines
        .iter()
        .skip(app.wt.pos.offset)
        .take(height)
        .copied()
        .collect();
    let mut fields = std::collections::HashMap::new();
    let lines: Vec<Line> = visible
        .into_iter()
        .map(|l| match l {
            VLine::Row(r) => match rows.get_mut(r).and_then(Option::take) {
                Some(row) => {
                    let w = &app.snap.worktrees[r];
                    let (line, hit) = wt_line(row, w, &shown_path(w), &cols, wide, width, now);
                    fields.insert(r, hit);
                    line
                }
                None => Line::default(),
            },
            VLine::Inset(r, item) => {
                let mut spans = vec![Span::raw("    ")];
                spans.extend(inset_spans(app, Focus::Worktrees, r, item));
                Line::from(spans)
            }
        })
        .collect();
    app.wt_fields = fields;
    let sel = app.wt.pos.sel.checked_sub(app.wt.pos.offset);
    paint_rows(f, inner, lines, sel, true);
}

// ---------------------------------------------------------------------------------------------
// Graph panel

/// No red: that's for remote labels (and conflicts, blocked agents).
const LANE_COLORS: [Color; 6] = [
    Color::Blue,
    Color::Magenta,
    Color::Green,
    Color::Yellow,
    Color::Cyan,
    Color::Indexed(208), // orange
];

fn lane_style(color: u8) -> Style {
    if color == graph::NO_COLOR {
        Style::new()
    } else {
        Style::new().fg(LANE_COLORS[color as usize % LANE_COLORS.len()])
    }
}

/// Compact relative age: `45s`, `12m`, `3h`, `5d`, `7w`, `4mo`, `2y`.
pub fn age(then: i64, now: i64) -> String {
    let d = (now - then).max(0);
    match d {
        d if d < 60 => format!("{d}s"),
        d if d < 3600 => format!("{}m", d / 60),
        d if d < 86400 => format!("{}h", d / 3600),
        d if d < 86400 * 14 => format!("{}d", d / 86400),
        d if d < 86400 * 60 => format!("{}w", d / (86400 * 7)),
        d if d < 86400 * 365 => format!("{}mo", d / (86400 * 30)),
        d => format!("{}y", d / (86400 * 365)),
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `[name]` in the worktree's lane color.
fn worktree_label(name: &str, color: u8) -> Span<'static> {
    Span::styled(format!("[{name}]"), lane_style(color).add_modifier(Modifier::BOLD))
}

/// The lane color of a worktree in the graph (its uncommitted row or HEAD commit).
pub fn worktree_color(app: &App, w: &WorktreeView) -> Option<u8> {
    let g = &app.snap.graph;
    let r = *g.worktree_row.get(&w.path)?;
    Some(g.rows.get(r)?.color).filter(|&c| c != graph::NO_COLOR)
}

fn cells_spans(cells: &[graph::Cell]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    // Group consecutive cells of the same color into one span.
    for cell in cells {
        let style = lane_style(cell.color);
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(cell.ch),
            _ => spans.push(Span::styled(cell.ch.to_string(), style)),
        }
    }
    spans
}

/// Below this panel width the graph leaves out the author column.
const AUTHOR_MIN_WIDTH: usize = 100;
const AUTHOR_MAX: usize = 18;

/// Columns available for the subject on a commit row whose prefix is `prefix` wide.
fn subject_room(prefix: usize, width: usize) -> usize {
    let author = if width >= AUTHOR_MIN_WIDTH { AUTHOR_MAX + 2 } else { 0 };
    width.saturating_sub(prefix + author)
}

/// Everything on a commit row before the subject: lanes, sha, age, labels.
fn commit_prefix(app: &App, row: &Row, ci: usize, now: i64) -> Vec<Span<'static>> {
    let g = &app.snap.graph;
    let c = &g.commits[ci];
    let dim = Style::new().fg(C_DIM);
    let mut spans = cells_spans(&row.cells);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        c.id[..c.id.len().min(7)].to_string(),
        Style::new().fg(C_HASH),
    ));
    spans.push(Span::styled(format!(" {:>3} ", age(c.time, now)), dim));
    let mut refs: Vec<Span> = Vec::new();
    for l in &g.labels[ci] {
        match l.kind {
            LabelKind::Worktree => {
                spans.push(worktree_label(&l.text, row.color));
                let wi = app
                    .snap
                    .worktrees
                    .iter()
                    .position(|w| w.name == l.text && w.prunable.is_none());
                if let Some(wi) = wi {
                    if let Some(act) = app.agent_activity(wi) {
                        spans.extend(agent_spans(act, false));
                    }
                    if let Some(c) = cleanup_span(app.cleanup(wi)) {
                        spans.extend([c, Span::raw(" ")]);
                    }
                }
                spans.push(Span::raw(" "));
            }
            kind => {
                // Branches take their lane's color; tags stay yellow.
                let (text, style) = match kind {
                    LabelKind::Branch => (l.text.clone(), lane_style(row.color)),
                    LabelKind::Remote => (l.text.clone(), Style::new().fg(C_REMOTE)),
                    _ => (format!("tag: {}", l.text), Style::new().fg(Color::Yellow)),
                };
                if !refs.is_empty() {
                    refs.push(Span::styled(", ", dim));
                }
                refs.push(Span::styled(text, style));
            }
        }
    }
    if !refs.is_empty() {
        spans.push(Span::styled("(", dim));
        spans.extend(refs);
        spans.push(Span::styled(") ", dim));
    }
    spans
}

/// Column where inset content starts under graph row `row`.
fn inset_col(row: &Row) -> usize {
    row.cells.len().max(row.below.len()) + 2
}

/// Wrapped subject and message of graph row `r` for a panel `width` wide (`None` before the
/// first render, when the width is unknown).
pub(super) fn inset_text(app: &App, r: usize, width: usize) -> Option<InsetText> {
    if width == 0 {
        return None;
    }
    let g = &app.snap.graph;
    let row = g.rows.get(r)?;
    // Continuation lines of the subject start at the same column as the message body.
    let body = width.saturating_sub(inset_col(row));
    let subject = match row.kind {
        RowKind::Commit(ci) => {
            let room = subject_room(spans_width(&commit_prefix(app, row, ci, now_secs())), width);
            inset::wrap(&g.commits[ci].subject, room, body)
        }
        RowKind::Uncommitted(_) => Vec::new(),
    };
    let message = app
        .row_doc(Focus::Graph, r)
        .filter(|e| e.doc.is_some())
        .map(|e| e.message.iter().flat_map(|l| inset::wrap(l, body, body)).collect());
    Some(InsetText { subject, message })
}

fn graph_line(app: &App, r: usize, now: i64, width: usize) -> Line<'static> {
    let g = &app.snap.graph;
    let row = &g.rows[r];
    match row.kind {
        RowKind::Commit(ci) => {
            let c = &g.commits[ci];
            let mut spans = commit_prefix(app, row, ci, now);
            // An open row wraps its subject onto continuation lines; otherwise it is truncated.
            let subject = match app.graph.texts.get(&r).and_then(|t| t.subject.first()) {
                Some(first) => first.clone(),
                None if width >= AUTHOR_MIN_WIDTH => truncate(&c.subject, subject_room(spans_width(&spans), width)),
                None => c.subject.clone(),
            };
            if width >= AUTHOR_MIN_WIDTH {
                // Subject, then the author right-aligned.
                let author = truncate(&c.author, AUTHOR_MAX);
                let gap = width.saturating_sub(spans_width(&spans) + subject.width() + author.width());
                spans.push(Span::raw(subject));
                spans.push(Span::raw(" ".repeat(gap)));
                spans.push(Span::styled(author, Style::new().fg(C_DIM)));
            } else {
                spans.push(Span::raw(subject));
            }
            Line::from(spans)
        }
        RowKind::Uncommitted(wi) => {
            let mut spans = cells_spans(&row.cells);
            spans.push(Span::raw(" "));
            if let Some(w) = app.snap.worktrees.get(wi) {
                spans.push(worktree_label(&w.name, row.color));
                if let Some(act) = app.agent_activity(wi) {
                    spans.extend(agent_spans(act, false));
                }
                if let Some(c) = cleanup_span(app.cleanup(wi)) {
                    spans.extend([c, Span::raw(" ")]);
                }
                spans.push(Span::raw(" "));
                spans.extend(count_spans(w));
                spans.push(Span::styled(
                    " uncommitted",
                    Style::new().fg(Color::Red).add_modifier(Modifier::ITALIC),
                ));
            }
            Line::from(spans)
        }
    }
}

fn draw_graph(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Graph && !app.shell_focus;
    let mut title = vec![Span::raw(" graph ")];
    if let Some(b) = &app.snap.base {
        title.push(Span::styled(format!("· base {b} "), Style::new().fg(C_DIM)));
    }
    let block = panel(Line::from(title), focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let height = inner.height as usize;
    let g = app.snap.graph.clone();
    if g.rows.is_empty() {
        let msg = if app.snap.statuses_loaded && g.commits.is_empty() && app.snap.error.is_none() {
            " no commits yet"
        } else {
            " loading…"
        };
        f.render_widget(Paragraph::new(Span::styled(msg, Style::new().fg(C_DIM))), inner);
        return;
    }

    app.set_wrap_width(inner.width as usize);
    let len = app.graph.lines.len();
    if let Some(path) = app.graph_jump.take()
        && let Some(&row) = g.worktree_row.get(&path)
        && let Some(idx) = inset::index_of_row(&app.graph.lines, row)
    {
        let pos = &mut app.graph.pos;
        pos.sel = idx;
        pos.follow = false;
        // Only scroll when the row is not comfortably visible; then put it a third from the top.
        let margin = (height / 4).min(3);
        if idx < pos.offset + margin || idx + margin >= pos.offset + height {
            pos.offset = idx.saturating_sub(height / 3);
        }
        pos.offset = pos.offset.min(len.saturating_sub(height));
    }
    app.graph.pos.clamp(len);
    app.graph.pos.scroll_into_view(height);

    let now = now_secs();
    let width = inner.width as usize;
    let visible: Vec<VLine> = app
        .graph
        .lines
        .iter()
        .skip(app.graph.pos.offset)
        .take(height)
        .copied()
        .collect();
    let mut lines: Vec<Line> = visible
        .into_iter()
        .map(|l| match l {
            VLine::Row(r) => graph_line(app, r, now, width),
            VLine::Inset(r, item) => {
                // Keep drawing the lanes that pass by, then the inset content.
                let row = &g.rows[r];
                let text = app.graph.texts.get(&r);
                let (col, content) = match (item, text) {
                    (Item::Subject(k), Some(t)) => {
                        let line = t.subject.get(k).cloned().unwrap_or_default();
                        (inset_col(row), vec![Span::raw(line)])
                    }
                    (Item::Message(i), Some(InsetText { message: Some(m), .. })) => {
                        let line = m.get(i).cloned().unwrap_or_default();
                        let style = Style::new().fg(C_DIM).add_modifier(Modifier::ITALIC);
                        (inset_col(row), vec![Span::styled(line, style)])
                    }
                    _ => (inset_col(row), inset_spans(app, Focus::Graph, r, item)),
                };
                let mut spans = cells_spans(&row.below);
                spans.push(Span::raw(" ".repeat(col.saturating_sub(row.below.len()))));
                spans.extend(content);
                Line::from(spans)
            }
        })
        .collect();
    if g.truncated && lines.len() < height {
        lines.push(Line::from(Span::styled(
            format!(" … older history not loaded yet ({} commits shown)", g.commits.len()),
            Style::new().fg(C_DIM),
        )));
    }
    let sel = app.graph.pos.sel.checked_sub(app.graph.pos.offset);
    paint_rows(f, inner, lines, sel, true);
}

// ---------------------------------------------------------------------------------------------
// Footer and help

pub fn tilde(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => format!("~{}", &s[home.len()..]),
        _ => s,
    }
}

fn worktree_details(app: &App, wi: usize) -> Option<Line<'static>> {
    let w = app.snap.worktrees.get(wi)?;
    let dim = Style::new().fg(C_DIM);
    let mut spans = vec![Span::raw(tilde(&w.path))];
    let sep = || Span::styled("  ·  ", dim);
    if let Some(h) = &w.head {
        spans.push(sep());
        spans.push(Span::styled(h[..h.len().min(8)].to_string(), Style::new().fg(C_HASH)));
    }
    match (&w.branch, w.status.as_ref().and_then(|s| s.upstream.clone())) {
        (Some(b), Some(up)) => {
            spans.push(sep());
            spans.push(Span::raw(format!("{b} → {up}")));
        }
        (None, _) if !w.bare => {
            spans.push(sep());
            spans.push(Span::styled("detached", dim));
        }
        _ => {}
    }
    if let Some(ch) = &w.changes
        && !ch.files.is_empty()
    {
        spans.push(sep());
        let n = ch.files.len();
        spans.push(Span::raw(format!("{n} file{} ", if n == 1 { "" } else { "s" })));
        spans.extend(add_del_spans(ch.adds as usize, ch.dels as usize));
        spans.push(Span::styled(" since fork", dim));
    }
    if !w.overlaps.is_empty() {
        // `⚠ feat-x: src/a.rs, src/b.rs +2 · main: README.md`
        spans.push(sep());
        let style = Style::new().fg(C_OVERLAP);
        spans.push(Span::styled("⚠ ", style));
        for (i, o) in w.overlaps.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", dim));
            }
            spans.push(Span::styled(format!("{}: ", o.with), style));
            let shown: Vec<&str> = o.files.iter().take(3).map(String::as_str).collect();
            spans.push(Span::raw(shown.join(", ")));
            if o.files.len() > 3 {
                spans.push(Span::styled(format!(" +{}", o.files.len() - 3), dim));
            }
        }
    }
    let base = app.snap.base.as_deref().unwrap_or("base");
    let landed = match w.landed {
        Some(Landed::Merged) => Some(format!("merged into {base}")),
        Some(Landed::Applied) => Some(format!("in {base} (squashed or rebased)")),
        Some(Landed::Unused) => Some("no commits of its own".to_string()),
        None => None,
    };
    if let Some(text) = landed {
        spans.push(sep());
        let style = if w.is_merged() { Style::new().fg(Color::Green) } else { dim };
        spans.push(Span::styled(text, style));
    }
    match app.cleanup(wi) {
        Cleanup::Safe => spans.push(Span::styled(", safe to clean up (Backspace)", Style::new().fg(Color::Green))),
        Cleanup::Untracked => spans.push(Span::styled(
            ", cleaning up would lose untracked files",
            Style::new().fg(C_MODIFIED),
        )),
        Cleanup::No => {}
    }
    if app.snap.watching && !w.watched && w.has_worktree() {
        spans.push(sep());
        spans.push(Span::styled("not watched: polled", dim));
    }
    if let Some(r) = &w.locked {
        spans.push(sep());
        let r = if r.is_empty() { String::new() } else { format!(": {r}") };
        spans.push(Span::styled(format!("locked{r}"), Style::new().fg(C_MODIFIED)));
    }
    if let Some(e) = &w.status_error {
        spans.push(sep());
        spans.push(Span::styled(
            e.lines().next().unwrap_or("").to_string(),
            Style::new().fg(C_CONFLICT),
        ));
    }
    Some(Line::from(spans))
}

fn graph_details(app: &App) -> Option<Line<'static>> {
    let g = &app.snap.graph;
    let row = app.graph.lines.get(app.graph.pos.sel)?.row();
    match g.rows.get(row)?.kind {
        RowKind::Uncommitted(wi) => worktree_details(app, wi),
        RowKind::Commit(ci) => {
            let c = &g.commits[ci];
            let dim = Style::new().fg(C_DIM);
            let mut spans = vec![
                Span::styled(c.id[..c.id.len().min(10)].to_string(), Style::new().fg(C_HASH)),
                Span::styled("  ·  ", dim),
                Span::raw(c.author.clone()),
                Span::styled("  ·  ", dim),
                Span::raw(format!("{} ({} ago)", local_date(c.time), age(c.time, now_secs()))),
            ];
            if c.parents.len() > 1 {
                spans.push(Span::styled(format!("  ·  merge of {}", c.parents.len()), dim));
            }
            Some(Line::from(spans))
        }
    }
}

/// `herdr: 3 agents, 1 blocked · ` for the agents working in this repo's worktrees (empty when
/// herdr isn't connected).
fn herdr_summary(app: &App) -> Vec<Span<'static>> {
    if app.herdr.is_none() {
        return Vec::new();
    }
    let dim = Style::new().fg(C_DIM);
    let acts: Vec<herdr::Activity> = (0..app.snap.worktrees.len())
        .filter_map(|i| app.agent_activity(i))
        .collect();
    let total: usize = acts.iter().map(|a| a.count).sum();
    let blocked = app.agents_in_state(AgentState::Blocked);
    let mut spans = vec![Span::styled(
        match total {
            0 => "herdr: no agents".to_string(),
            1 => "herdr: 1 agent".to_string(),
            n => format!("herdr: {n} agents"),
        },
        dim,
    )];
    if blocked > 0 {
        spans.push(Span::styled(", ", dim));
        spans.push(Span::styled(
            format!("{blocked} blocked"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(" · ", dim));
    spans
}

/// Footer summary of how the view stays fresh: `watching 8/9 · poll 60s`, `no poll`, ….
fn refresh_mode(app: &App) -> String {
    let trees = app.snap.worktrees.iter().filter(|w| w.has_worktree()).count();
    let watched = app.snap.worktrees.iter().filter(|w| w.watched).count();
    let watch = if !app.snap.watching {
        String::new()
    } else if watched > 0 {
        format!("watching {watched}/{trees} · ")
    } else {
        "watching git · ".into()
    };
    let poll = if app.snap.polling {
        format!("poll {}", secs(app.interval_ms))
    } else {
        "no poll (r)".into()
    };
    format!("{watch}{poll} ")
}

/// `60s`, `1.5s`: an interval without a pointless `.0`.
fn secs(ms: u128) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// `YYYY-MM-DD HH:MM EDT` in the local timezone (`TZ`, else `/etc/localtime`, via libc), or
/// [`utc_date`] when libc can't convert.
pub fn local_date(t: i64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    if unsafe { libc::localtime_r(&tt, &mut tm) }.is_null() {
        return utc_date(t);
    }
    let zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned()
    };
    let zone = if zone.is_empty() {
        let off = tm.tm_gmtoff / 60;
        format!("{}{:02}{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 60, off.abs() % 60)
    } else {
        zone
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} {zone}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// `YYYY-MM-DD HH:MM UTC` from unix seconds (civil-from-days, no timezone database needed).
pub fn utc_date(t: i64) -> String {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", secs / 3600, secs % 3600 / 60)
}

/// Style for one line of `git show` / `git diff` output.
fn diff_line_style(line: &str) -> Style {
    let b = line.as_bytes();
    if line.starts_with("diff --git ") || line.starts_with("diff --cc ") {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else if line.starts_with("── ") {
        // A side's heading in the overlaps view.
        Style::new().fg(C_OVERLAP).add_modifier(Modifier::BOLD)
    } else if line.starts_with("commit ") {
        Style::new().fg(Color::Yellow)
    } else if line == "---" {
        Style::new().fg(C_DIM)
    } else if line.starts_with("+++ ") || line.starts_with("--- ") {
        Style::new().add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::new().fg(Color::Cyan)
    } else if b.first() == Some(&b'+') {
        Style::new().fg(Color::Green)
    } else if b.first() == Some(&b'-') {
        Style::new().fg(Color::Red)
    } else if [
        "index ",
        "new file",
        "deleted file",
        "similarity ",
        "rename ",
        "old mode",
        "new mode",
        "Binary ",
    ]
    .iter()
    .any(|p| line.starts_with(p))
    {
        Style::new().fg(C_DIM)
    } else if line.starts_with("Untracked files") {
        Style::new().fg(C_UNTRACKED).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    }
}

/// Diffstat lines (` path | 12 +++--`) get colored plus/minus bars.
fn stat_line(line: &str) -> Option<Line<'static>> {
    let (path, rest) = line.rsplit_once(" | ")?;
    if !line.starts_with(' ') {
        return None;
    }
    let bar_start = rest.find(['+', '-'])?;
    let (num, bar) = rest.split_at(bar_start);
    if !bar.chars().all(|c| c == '+' || c == '-') {
        return None;
    }
    let plus = bar.chars().filter(|&c| c == '+').count();
    Some(Line::from(vec![
        Span::raw(format!("{path} | {num}")),
        Span::styled("+".repeat(plus), Style::new().fg(Color::Green)),
        Span::styled("-".repeat(bar.len() - plus), Style::new().fg(Color::Red)),
    ]))
}

/// Drop the first `cols` display columns of `s`.
fn skip_cols(s: &str, cols: usize) -> &str {
    if cols == 0 {
        return s;
    }
    let mut w = 0;
    for (i, c) in s.char_indices() {
        if w >= cols {
            return &s[i..];
        }
        w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
    }
    ""
}

fn status_color(status: char) -> Color {
    match status {
        'A' => Color::Green,
        'D' => Color::Red,
        'R' | 'C' => Color::Blue,
        'U' => C_CONFLICT,
        '?' => C_UNTRACKED,
        _ => Color::Yellow,
    }
}

/// `+12 -3` with colors (zero sides omitted).
fn add_del_spans(adds: usize, dels: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    if adds > 0 {
        out.push(Span::styled(format!("+{adds}"), Style::new().fg(Color::Green)));
    }
    if dels > 0 {
        if !out.is_empty() {
            out.push(Span::raw(" "));
        }
        out.push(Span::styled(format!("-{dels}"), Style::new().fg(Color::Red)));
    }
    out
}

/// The file list as a table (the overlaps view): status, path, then one `+a -d` column per side.
struct FileTable {
    path: usize,
    cols: Vec<usize>,
}

impl FileTable {
    /// Paths get what the side columns leave, at least this much (cut from the left; the full
    /// path is in the diff pane's title).
    const MIN_PATH: usize = 16;
    const MAX_HEADER: usize = 24;
    /// Status letter and its padding before the path.
    const LEAD: usize = 4;

    fn new(columns: &[String], files: &[FileDiff], width: usize) -> Self {
        let cols: Vec<usize> = columns
            .iter()
            .enumerate()
            .map(|(k, name)| {
                let cells = files
                    .iter()
                    .map(|f| spans_width(&Self::cell(f.cells.get(k).copied().flatten())));
                cells.fold(name.width().min(Self::MAX_HEADER), usize::max)
            })
            .collect();
        let used = Self::LEAD + cols.iter().map(|w| w + 2).sum::<usize>();
        let longest = files.iter().map(|f| f.path.width()).max().unwrap_or(0).max(4);
        let path = longest.min(width.saturating_sub(used).max(Self::MIN_PATH));
        FileTable { path, cols }
    }

    /// `+a -d`; `·` where that side didn't touch the file, `±0` where it no longer differs (or
    /// only binary content changed).
    fn cell(c: Option<(usize, usize)>) -> Vec<Span<'static>> {
        let dim = Style::new().fg(C_DIM);
        match c {
            None => vec![Span::styled("·", dim)],
            Some((0, 0)) => vec![Span::styled("±0", dim)],
            Some((a, d)) => add_del_spans(a, d),
        }
    }

    fn header(&self, columns: &[String]) -> Line<'static> {
        let dim = Style::new().fg(C_DIM);
        let mut spans = vec![
            Span::raw(" ".repeat(Self::LEAD)),
            Span::styled(pad("file".into(), self.path), dim),
        ];
        for (k, (name, w)) in columns.iter().zip(&self.cols).enumerate() {
            // This worktree first, then the sides it overlaps with.
            let style = match k {
                0 => Style::new().add_modifier(Modifier::BOLD),
                _ => Style::new().fg(C_OVERLAP).add_modifier(Modifier::BOLD),
            };
            spans.push(Span::raw("  "));
            spans.push(Span::styled(pad(truncate(name, *w), *w), style));
        }
        Line::from(spans)
    }

    fn row(&self, file: &FileDiff) -> Line<'static> {
        let mut spans = vec![
            Span::styled(format!(" {} ", file.status), Style::new().fg(status_color(file.status))),
            Span::raw(" "),
            Span::raw(pad(truncate_left(&file.path, self.path), self.path)),
        ];
        for (k, w) in self.cols.iter().enumerate() {
            let cell = Self::cell(file.cells.get(k).copied().flatten());
            let cw = spans_width(&cell);
            spans.push(Span::raw("  "));
            spans.extend(cell);
            spans.push(Span::raw(" ".repeat(w.saturating_sub(cw))));
        }
        Line::from(spans)
    }
}

fn draw_diff(f: &mut Frame, app: &mut App, area: Rect) {
    // Files that other worktrees changed too (for a worktree's diff): file → their names. The
    // overlaps view itself names the other side on each entry instead.
    let overlapping: std::collections::HashMap<String, Vec<String>> = app
        .diff
        .as_ref()
        .filter(|d| !matches!(d.target, crate::diff::Target::Overlaps { .. }))
        .and_then(|d| d.target.worktree())
        .and_then(|p| app.snap.worktrees.iter().find(|w| w.path == p))
        .map(|w| {
            let mut m: std::collections::HashMap<String, Vec<String>> = Default::default();
            for o in &w.overlaps {
                for file in &o.files {
                    m.entry(file.clone()).or_default().push(o.with.clone());
                }
            }
            m
        })
        .unwrap_or_default();
    let Some(d) = app.diff.as_mut() else { return };
    let dim = Style::new().fg(C_DIM);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(d.title.clone(), bold),
        Span::raw(" "),
    ]);

    // File list on top (only when there are files), the selected entry's diff below.
    let body_area = if d.has_files() {
        let header = d.header_rows();
        let want = (d.entries() + header) as u16 + CHROME;
        let h = want
            .min((area.height * 2 / 5).max(5))
            .min(area.height.saturating_sub(3));
        let [list, body] = Layout::vertical([Constraint::Length(h), Constraint::Min(0)]).areas(area);
        d.list_area = list;

        let (adds, dels) = d.files().iter().fold((0, 0), |(a, r), f| (a + f.adds, r + f.dels));
        let n = d.files().len();
        let mut summary = vec![Span::styled(
            format!(" {n} file{} ", if n == 1 { "" } else { "s" }),
            dim,
        )];
        summary.extend(add_del_spans(adds, dels));
        summary.push(Span::raw(" "));
        let block = panel(title.clone(), d.pane == Pane::Files).title_top(Line::from(summary).right_aligned());
        let inner = block.inner(list);
        f.render_widget(block, list);
        let [head_area, inner] = Layout::vertical([Constraint::Length(header as u16), Constraint::Min(0)]).areas(inner);

        let rows = inner.height as usize;
        if d.sel < d.list_offset {
            d.list_offset = d.sel;
        } else if rows > 0 && d.sel >= d.list_offset + rows {
            d.list_offset = d.sel + 1 - rows;
        }
        let overview_label = if d.is_commit() {
            "commit message & stats"
        } else {
            "summary"
        };
        let width = inner.width as usize;
        // Counts form a column right after the longest path (not pinned to the far edge).
        let counts_w = d
            .files()
            .iter()
            .map(|f| spans_width(&add_del_spans(f.adds, f.dels)))
            .max()
            .unwrap_or(0);
        let longest = d.files().iter().map(|f| f.path.width()).max().unwrap_or(0);
        let path_w = longest.min(width.saturating_sub(4 + counts_w + 2));
        let table = (header > 0).then(|| FileTable::new(d.columns(), d.files(), width));
        if let Some(t) = &table {
            f.render_widget(Paragraph::new(t.header(d.columns())), head_area);
        }
        let lines: Vec<Line> = (d.list_offset..d.entries())
            .take(rows)
            .map(|i| match i.checked_sub(1).map(|fi| &d.files()[fi]) {
                None => Line::from(vec![Span::styled(" ≡ ", dim), Span::styled(overview_label, dim)]),
                Some(file) if table.is_some() => table.as_ref().unwrap().row(file),
                Some(file) => {
                    let counts = add_del_spans(file.adds, file.dels);
                    let path = pad(truncate(&file.path, path_w), path_w);
                    let mut spans = vec![
                        Span::styled(format!(" {} ", file.status), Style::new().fg(status_color(file.status))),
                        Span::raw(" "),
                        Span::raw(path),
                        Span::raw("  "),
                    ];
                    let cw = spans_width(&counts);
                    spans.extend(counts);
                    let also = overlapping.get(&file.path).or_else(|| {
                        // An untracked directory in the overlap stands for the files under it.
                        overlapping
                            .iter()
                            .find(|(k, _)| k.ends_with('/') && file.path.starts_with(k.as_str()))
                            .map(|(_, v)| v)
                    });
                    if let Some(names) = also {
                        spans.push(Span::raw(" ".repeat(counts_w - cw + 2)));
                        spans.push(Span::styled(
                            format!("⚠ also in {}", names.join(", ")),
                            Style::new().fg(C_OVERLAP),
                        ));
                    }
                    Line::from(spans)
                }
            })
            .collect();
        let sel = d.sel.checked_sub(d.list_offset);
        paint_rows(f, inner, lines, sel, d.pane == Pane::Files);
        body
    } else {
        d.list_area = Rect::default();
        area
    };
    d.body_area = body_area;

    let height = content(body_area).height as usize;
    let body = d.body();
    let label: Vec<Span> = match d.selected_file() {
        _ if !d.has_files() => title.spans.clone(),
        Some(file) => vec![
            Span::raw(" "),
            Span::styled(file.status.to_string(), Style::new().fg(status_color(file.status))),
            Span::raw(" "),
            Span::styled(file.path.clone(), bold),
            Span::raw(" "),
        ],
        None => vec![
            Span::raw(" "),
            Span::styled(if d.is_commit() { "commit" } else { "summary" }, bold),
            Span::raw(" "),
        ],
    };
    let position = if body.len() > height {
        let end = (d.scroll + height).min(body.len());
        format!(" {}–{} of {} ", d.scroll + 1, end, body.len())
    } else {
        String::new()
    };
    let focused = d.pane == Pane::Body || !d.has_files();
    let block = panel(Line::from(label), focused).title_top(Line::styled(position, dim).right_aligned());
    let inner = block.inner(body_area);
    f.render_widget(block, body_area);
    let lines: Vec<Line> = match &d.doc {
        None => vec![Line::styled(" loading…", dim)],
        Some(Err(e)) => e
            .lines()
            .map(|l| Line::styled(format!(" {l}"), Style::new().fg(C_CONFLICT)))
            .collect(),
        Some(Ok(_)) => body[d.scroll.min(body.len())..]
            .iter()
            .take(height)
            .map(|l| {
                let visible = skip_cols(l, d.hscroll);
                stat_line(visible).unwrap_or_else(|| Line::styled(visible.to_string(), diff_line_style(l)))
            })
            .collect(),
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_diff_footer(f: &mut Frame, app: &App, area: Rect) {
    let [details, hints] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let Some(d) = &app.diff else { return };
    let mut msg = match &d.target {
        crate::diff::Target::Commit { id, .. } => id.clone(),
        t => t.worktree().map(tilde).unwrap_or_default(),
    };
    if d.hscroll > 0 {
        msg.push_str(&format!("  ·  scrolled right {} columns", d.hscroll));
    }
    let details_line = match &app.notice {
        Some(n) => Span::styled(format!(" {n}"), Style::new().fg(C_MODIFIED)),
        None => Span::styled(format!(" {msg}"), Style::new().fg(C_DIM)),
    };
    f.render_widget(Paragraph::new(details_line), details);
    if app.copy_pending {
        f.render_widget(Paragraph::new(copy_hints(app)), hints);
        return;
    }

    let key = |k: &'static str| Span::styled(k, Style::new().fg(Color::Cyan));
    let text = |t: &'static str| Span::styled(t, Style::new().fg(C_DIM));
    let mut spans = vec![Span::raw(" ")];
    if d.pane == Pane::Files && d.has_files() {
        spans.extend([key("↑↓"), text(" file  "), key("Enter/Tab"), text(" into diff  ")]);
    } else {
        spans.extend([key("↑↓"), text(" scroll  ")]);
        if d.has_files() {
            spans.extend([key("n/p"), text(" file  "), key("Tab"), text(" file list  ")]);
        }
    }
    spans.extend([
        key("PgUp/PgDn"),
        text(" page  "),
        key("←→"),
        text(" pan  "),
        key("r"),
        text(" reload  "),
        key("y"),
        text(" copy  "),
        key("q"),
        text(" back"),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), hints);
}

/// The inset shell: its screen, drawn cell by cell, and the cursor while it has the keys.
fn draw_shell(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(shell) = app.shell.as_mut() else { return };
    let focused = app.shell_focus;
    let dim = Style::new().fg(C_DIM);
    let hint = match (focused, shell.label.as_str()) {
        (true, "shell") => " Ctrl-] back to canopy · exit to close ".to_string(),
        (true, label) => format!(" Ctrl-] back to canopy · quit {label} to close "),
        (false, _) => " click or Tab to type ".to_string(),
    };
    // `── shell · feat-auth ~/…/wt/feat-auth ──── hint`, the path shortened to fit.
    let name = app
        .snap
        .worktrees
        .iter()
        .find(|w| w.path == shell.term.path)
        .map(|w| w.name.clone());
    let mut title = vec![Span::raw(format!(" {} ", shell.label))];
    if let Some(n) = &name {
        title.push(Span::styled("· ", dim));
        title.push(Span::styled(format!("{n} "), Style::new().add_modifier(Modifier::BOLD)));
    }
    let room = (area.width as usize).saturating_sub(spans_width(&title) + hint.width() + 6);
    if room >= 8 {
        title.push(Span::styled(
            format!("{} ", truncate_left(&tilde(&shell.term.path), room)),
            dim,
        ));
    }
    let title = Line::from(title);
    let block = panel(title, focused).title_top(Line::styled(hint, dim).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    shell.term.resize(inner.height, inner.width);
    let parser = shell.term.screen();
    let screen = parser.screen();
    let buf = f.buffer_mut();
    for row in 0..inner.height {
        for col in 0..inner.width {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut style = Style::new()
                .fg(term_color(cell.fgcolor()))
                .bg(term_color(cell.bgcolor()));
            for (on, m) in [
                (cell.bold(), Modifier::BOLD),
                (cell.dim(), Modifier::DIM),
                (cell.italic(), Modifier::ITALIC),
                (cell.underline(), Modifier::UNDERLINED),
                (cell.inverse(), Modifier::REVERSED),
            ] {
                if on {
                    style = style.add_modifier(m);
                }
            }
            let text = if cell.has_contents() { cell.contents() } else { " " };
            buf[(inner.x + col, inner.y + row)].set_symbol(text).set_style(style);
        }
    }
    if focused && !screen.hide_cursor() && screen.scrollback() == 0 {
        let (row, col) = screen.cursor_position();
        if row < inner.height && col < inner.width {
            f.set_cursor_position((inner.x + col, inner.y + row));
        }
    }
}

fn term_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [details, hints] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let key = |k: &'static str| Span::styled(k, Style::new().fg(Color::Cyan));
    let text = |t: &'static str| Span::styled(t, Style::new().fg(C_DIM));
    if let Some(n) = &app.notice {
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {n}"), Style::new().fg(C_MODIFIED))),
            details,
        );
    }
    if app.copy_pending {
        f.render_widget(Paragraph::new(copy_hints(app)), hints);
        return;
    }
    if app.shell_focus {
        let label = app.shell.as_ref().map_or("shell", |s| s.label.as_str());
        let mut spans = vec![Span::raw(" "), key("Ctrl-]"), text(" back to canopy  ")];
        if label == "shell" {
            spans.extend([key("exit"), text(" close the shell  "), text("keys go to the shell")]);
        } else {
            spans.push(Span::styled(
                format!("quit {label} to close it  keys go to {label}"),
                Style::new().fg(C_DIM),
            ));
        }
        let line = Line::from(spans);
        f.render_widget(Paragraph::new(line), hints);
        return;
    }
    let line = match app.focus {
        Focus::Worktrees => app
            .wt
            .lines
            .get(app.wt.pos.sel)
            .and_then(|l| worktree_details(app, l.row())),
        Focus::Graph => graph_details(app),
    };
    if app.notice.is_some() {
        // Shown above instead of the details.
    } else if let Some(e) = &app.snap.error {
        let msg = format!(" {}", e.lines().next().unwrap_or(""));
        f.render_widget(Paragraph::new(Span::styled(msg, Style::new().fg(C_CONFLICT))), details);
    } else if let Some(mut line) = line {
        line.spans.insert(0, Span::raw(" "));
        f.render_widget(Paragraph::new(line), details);
    }

    // `o` only where it does something: on a worktree that overlaps with another.
    let overlaps = app
        .focused_wi()
        .and_then(|wi| app.snap.worktrees.get(wi))
        .is_some_and(|w| !w.overlaps.is_empty());
    let o_hint = if overlaps {
        vec![key("o"), text(" overlaps  ")]
    } else {
        Vec::new()
    };
    let left = Line::from(
        vec![
            Span::raw(" "),
            key("↑↓"),
            text(" move  "),
            key("Space"),
            text(" expand  "),
            key("Enter"),
            text(" full diff  "),
            key("b"),
            text(" vs base  "),
        ]
        .into_iter()
        .chain(o_hint)
        .chain([
            key("t"),
            text(" git TUI  "),
            key("Tab"),
            text(" panel  "),
            key("v"),
            text(" layout  "),
            key("y"),
            text(" copy  "),
            key("?"),
            text(" help  "),
            key("q"),
            text(" quit"),
        ])
        .collect::<Vec<_>>(),
    );
    let left_width = left.width();
    f.render_widget(Paragraph::new(left), hints);
    let mut right = herdr_summary(app);
    right.push(Span::styled(refresh_mode(app), Style::new().fg(C_DIM)));
    if left_width + spans_width(&right) < hints.width as usize {
        f.render_widget(Paragraph::new(Line::from(right).right_aligned()), hints);
    }
}

/// The footer while a `y` chord is pending: what each second key would copy here.
fn copy_hints(app: &App) -> Line<'static> {
    let ctx = app.copy_ctx();
    let key = |k: String| Span::styled(k, Style::new().fg(Color::Cyan));
    let text = |t: String| Span::styled(t, Style::new().fg(C_DIM));
    let mut spans = vec![Span::styled(" copy ", Style::new().add_modifier(Modifier::BOLD))];
    if let Some(d) = ctx.default {
        spans.extend([key("y".into()), text(format!(" {}  ", d.label()))]);
    }
    for (c, field) in copy::FIELDS {
        if ctx.has(field) {
            spans.extend([key(c.to_string()), text(format!(" {}  ", field.label()))]);
        }
    }
    spans.extend([key("Esc".into()), text(" cancel".into())]);
    Line::from(spans)
}

const HELP: &[(&str, &str)] = &[
    ("↑ ↓  j k", "move (through expanded lines too)"),
    ("PgUp PgDn  ^u ^d", "page / half page"),
    ("g G  Home End", "first / last"),
    ("Space  click", "expand / collapse row or file"),
    ("→ l  ← h", "expand / collapse"),
    ("Esc", "fold back to the row (again: fold all)"),
    ("Enter  dbl-click", "full diff view"),
    ("Tab", "switch panel"),
    ("v", "stacked / side-by-side layout"),
    ("a  dbl-click agent", "jump to the worktree's agent in herdr"),
    ("s  dbl-click path", "shell in the worktree (pane below, or inset)"),
    ("t", "git TUI in the worktree (lazygit, gitui, tig)"),
    ("b", "diff vs base: all changes since the fork"),
    ("o  dbl-click ⚠", "overlaps: both sides' changes to shared files"),
    ("Backspace", "clean up a ⌫ worktree: checkout, branch, herdr workspace"),
    ("Ctrl-]", "leave the inset shell (exit closes it)"),
    ("y y", "copy the sha / path / file under the cursor"),
    ("y h  y s", "copy sha / short sha"),
    ("y b  y m  y M", "copy branch / subject / message"),
    ("y p  y f  y d", "copy worktree path / file path / diff"),
    ("r", "refresh now"),
    ("?", "toggle this help"),
    ("q  Ctrl-C", "quit"),
    ("", ""),
    ("in diff view:", ""),
    ("↑ ↓  j k", "pick file / scroll diff"),
    ("Tab  Enter  Esc", "file list ⇄ diff"),
    ("n p  ] [", "next / previous file"),
    ("PgUp PgDn  Space", "scroll diff"),
    ("← →  h l", "scroll sideways"),
    ("r", "reload"),
    ("q", "back"),
];

/// The confirm popup for removing a worktree: exactly what will happen.
fn draw_confirm(f: &mut Frame, plan: &crate::cleanup::Plan) {
    let area = f.area();
    let w = area.width.min(88);
    let inner = w.saturating_sub(4) as usize;
    let dim = Style::new().fg(C_DIM);
    let bullet = |text: String| Line::from(vec![Span::styled(" • ", dim), Span::raw(truncate(&text, inner.saturating_sub(3)))]);
    let how = match plan.landed {
        Landed::Applied => "squashed or rebased",
        _ => "merged",
    };
    let mut lines = vec![
        Line::from(vec![
            Span::raw(" Remove "),
            Span::styled(plan.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("?"),
        ]),
        Line::styled(format!(" its work is in {} ({how}); nothing uncommitted", plan.base), dim),
        Line::raw(""),
    ];
    if let Some(ws) = &plan.workspace {
        let panes = if ws.panes == 1 { "1 pane".to_string() } else { format!("{} panes", ws.panes) };
        lines.push(bullet(format!("closes herdr workspace {} ({panes})", ws.label)));
    }
    // Long paths keep their end, the part that names the worktree.
    let removes = if plan.workspace.is_some() { "herdr removes " } else { "removes " };
    let room = inner.saturating_sub(3 + removes.len());
    lines.push(bullet(format!("{removes}{}", truncate_left(&tilde(&plan.path), room))));
    if plan.ignored_total > 0 {
        let more = plan.ignored_total.saturating_sub(plan.ignored.len());
        let mut list = plan.ignored.join(", ");
        if more > 0 {
            list.push_str(&format!(" +{more}"));
        }
        lines.push(Line::styled(
            truncate(&format!("     with ignored files: {list}"), inner),
            Style::new().fg(C_MODIFIED),
        ));
    }
    if let Some(b) = &plan.branch {
        lines.push(bullet(format!("deletes branch {b} (at {})", &plan.head[..plan.head.len().min(7)])));
    }
    for a in &plan.stray_agents {
        lines.push(Line::styled(
            truncate(&format!(" ! agent {a} stays open in a deleted directory"), inner),
            Style::new().fg(C_MODIFIED),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(" y", Style::new().fg(Color::Cyan)),
        Span::styled(" / ", dim),
        Span::styled("Enter", Style::new().fg(Color::Cyan)),
        Span::raw(" remove    "),
        Span::styled("Esc", Style::new().fg(Color::Cyan)),
        Span::raw(" cancel"),
    ]));
    let h = (lines.len() as u16 + 4).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(C_MODIFIED))
        .title(" clean up ")
        .padding(Padding::new(1, 1, 1, 1));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn draw_help(f: &mut Frame) {
    let area = f.area();
    let widest = HELP.iter().map(|(_, d)| d.width()).max().unwrap_or(0) as u16;
    let w = (1 + 19 + widest + 3).min(area.width); // " key  description " + frame
    let h = (HELP.len() as u16 + 4).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!(" {k:<19}"), Style::new().fg(Color::Cyan)),
                Span::raw(*d),
            ])
        })
        .collect();
    f.render_widget(Clear, rect);
    // A popup over other content keeps a full frame.
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border_style(true))
        .title(" keys ")
        .padding(Padding::vertical(1));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_ages() {
        assert_eq!(utc_date(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_date(1700003600), "2023-11-14 23:13 UTC");
        assert_eq!(utc_date(951782400), "2000-02-29 00:00 UTC");
        // Whatever the machine's timezone: date, time, then a zone name or offset.
        let local = local_date(1700003600);
        assert_eq!(local.as_bytes()[10], b' ', "{local}");
        assert!(local.starts_with("2023-11-1") && local.len() > 17, "{local}");
        assert_eq!(age(100, 100), "0s");
        assert_eq!(age(0, 7200), "2h");
        assert_eq!(age(0, 86400 * 3), "3d");
        assert_eq!(age(0, 86400 * 400), "1y");
    }

    #[test]
    fn common_parents() {
        use std::path::Path;
        let ps = [
            Path::new("/x/fx/repo"),
            Path::new("/x/fx/wt/a"),
            Path::new("/x/fx/wt/b"),
        ];
        assert_eq!(common_parent(ps.into_iter()).unwrap(), Path::new("/x/fx"));
        let nested = [Path::new("/r"), Path::new("/r/.claude/worktrees/a")];
        assert_eq!(common_parent(nested.into_iter()).unwrap(), Path::new("/"));
        assert_eq!(common_parent([Path::new("/only")].into_iter()), None);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }
}

#[cfg(test)]
mod diff_render_tests {
    use super::*;

    #[test]
    fn stat_lines() {
        let l = stat_line(" src/a.rs | 12 +++--").unwrap();
        assert_eq!(l.spans[1].content, "+++");
        assert_eq!(l.spans[2].content, "--");
        assert!(stat_line("+added | line").is_none());
        assert!(stat_line(" a | Bin 0 -> 12 bytes").is_none());
    }

    #[test]
    fn column_skipping() {
        assert_eq!(skip_cols("abcdef", 2), "cdef");
        assert_eq!(skip_cols("ab", 5), "");
        assert_eq!(skip_cols("日本語", 2), "本語");
    }
}
