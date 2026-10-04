//! Drawing. Pure functions of [`App`] state (plus scroll-offset bookkeeping).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::{App, Focus};
use crate::graph::{self, LabelKind, Row, RowKind};
use crate::model::WorktreeView;

pub const C_STAGED: Color = Color::Green;
pub const C_MODIFIED: Color = Color::Yellow;
pub const C_UNTRACKED: Color = Color::Magenta;
pub const C_CONFLICT: Color = Color::Red;
pub const C_DIM: Color = Color::DarkGray;
const SEL_FOCUSED: Color = Color::Indexed(238);
const SEL_UNFOCUSED: Color = Color::Indexed(235);

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(f.area());
    if app.diff.is_some() {
        app.areas.diff = main;
        draw_diff(f, app, main);
        draw_diff_footer(f, app, footer);
        if app.help {
            draw_help(f);
        }
        return;
    }
    let [left, right] = if main.width < NARROW {
        // Narrow terminal (e.g. a popup pane): stack the worktree list above the graph.
        let want = app.snap.worktrees.len().max(1) as u16 + 2;
        let h = want.min(main.height / 2).max(3.min(main.height));
        Layout::vertical([Constraint::Length(h), Constraint::Min(0)]).areas(main)
    } else {
        let wt_width = worktree_panel_width(app, main.width);
        Layout::horizontal([Constraint::Length(wt_width), Constraint::Min(10)]).areas(main)
    };
    app.areas.worktrees = left;
    app.areas.graph = right;
    draw_worktrees(f, app, left);
    draw_graph(f, app, right);
    draw_footer(f, app, footer);
    if app.help {
        draw_help(f);
    }
}

fn panel(title: Line<'static>, focused: bool) -> Block<'static> {
    let border = if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(C_DIM)
    };
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title)
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
    counts: Vec<Span<'static>>,
    ab: Vec<Span<'static>>,
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

/// Ahead/behind: vs the base branch (`↑a ↓b`), and vs upstream (`⇡a ⇣b`) when not in sync.
fn ab_spans(w: &WorktreeView, base: Option<&str>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let is_base = base.is_some() && w.branch.as_deref() == base;
    if !is_base && let Some((a, b)) = w.vs_base {
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

fn wt_rows(app: &App) -> Vec<WtRow> {
    let base = app.snap.base.as_deref();
    app.snap
        .worktrees
        .iter()
        .map(|w| {
            let mut style = Style::new();
            if w.is_current {
                style = style.add_modifier(Modifier::BOLD);
            }
            if w.prunable.is_some() || w.bare {
                style = style.fg(C_DIM);
            }
            WtRow {
                marker: marker(w),
                name: Span::styled(w.name.clone(), style),
                counts: count_spans(w),
                ab: ab_spans(w, base),
            }
        })
        .collect()
}

const NAME_MAX: usize = 28;
/// Below this width the panels are stacked vertically.
const NARROW: u16 = 72;

fn worktree_panel_width(app: &App, total: u16) -> u16 {
    let rows = wt_rows(app);
    let name = rows
        .iter()
        .map(|r| r.name.content.width())
        .max()
        .unwrap_or(4)
        .min(NAME_MAX);
    let counts = rows.iter().map(|r| spans_width(&r.counts)).max().unwrap_or(0);
    let ab = rows.iter().map(|r| spans_width(&r.ab)).max().unwrap_or(0);
    // borders + marker + gaps
    let natural = 2 + 2 + name + 1 + counts + if ab > 0 { 1 + ab } else { 0 } + 1;
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

fn draw_worktrees(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Worktrees;
    let n = app.snap.worktrees.len();
    let dirty = app.snap.worktrees.iter().filter(|w| w.is_dirty()).count();
    let mut title = vec![Span::raw(" worktrees ")];
    if n > 0 {
        title.push(Span::styled(format!("{n} "), Style::new().fg(C_DIM)));
        if dirty > 0 {
            title.push(Span::styled(format!("✎{dirty} "), Style::new().fg(C_MODIFIED)));
        }
    }
    let block = panel(Line::from(title), focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if n == 0 {
        f.render_widget(Paragraph::new(Span::styled(" loading…", Style::new().fg(C_DIM))), inner);
        return;
    }

    let rows = wt_rows(app);
    let avail = inner.width as usize;
    let counts_w = rows.iter().map(|r| spans_width(&r.counts)).max().unwrap_or(0);
    let ab_w = rows.iter().map(|r| spans_width(&r.ab)).max().unwrap_or(0);
    let name_natural = rows.iter().map(|r| r.name.content.width()).max().unwrap_or(0);
    let fixed = 2 + 1 + counts_w + if ab_w > 0 { 1 + ab_w } else { 0 };
    let name_w = name_natural.min(NAME_MAX).min(avail.saturating_sub(fixed).max(6));

    app.wt.clamp(n);
    app.wt.scroll_into_view(inner.height as usize);
    let lines: Vec<Line> = rows
        .into_iter()
        .skip(app.wt.offset)
        .take(inner.height as usize)
        .map(|r| {
            let mut spans = vec![r.marker, Span::raw(" ")];
            let name = pad(truncate(&r.name.content, name_w), name_w);
            spans.push(Span::styled(name, r.name.style));
            spans.push(Span::raw(" "));
            let cw = spans_width(&r.counts);
            spans.extend(r.counts);
            if ab_w > 0 {
                spans.push(Span::raw(" ".repeat(counts_w - cw + 1)));
                spans.extend(r.ab);
            }
            Line::from(spans)
        })
        .collect();
    let sel = app.wt.sel.checked_sub(app.wt.offset);
    paint_rows(f, inner, lines, sel, focused);
}

// ---------------------------------------------------------------------------------------------
// Graph panel

const LANE_COLORS: [Color; 6] = [
    Color::Blue,
    Color::Magenta,
    Color::Green,
    Color::Yellow,
    Color::Cyan,
    Color::Red,
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

fn worktree_label(name: &str) -> Span<'static> {
    Span::styled(
        format!("[{name}]"),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )
}

fn graph_line(app: &App, row: &Row, now: i64) -> Line<'static> {
    let g = &app.snap.graph;
    let mut spans: Vec<Span> = Vec::new();
    // Group consecutive cells of the same color into one span.
    for cell in &row.cells {
        let style = lane_style(cell.color);
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(cell.ch),
            _ => spans.push(Span::styled(cell.ch.to_string(), style)),
        }
    }
    spans.push(Span::raw(" "));
    match row.kind {
        RowKind::Commit(ci) => {
            let c = &g.commits[ci];
            spans.push(Span::styled(
                c.id[..c.id.len().min(7)].to_string(),
                Style::new().fg(Color::Yellow),
            ));
            spans.push(Span::styled(
                format!(" {:>3} ", age(c.time, now)),
                Style::new().fg(C_DIM),
            ));
            let labels = &g.labels[ci];
            let mut refs: Vec<Span> = Vec::new();
            for l in labels {
                match l.kind {
                    LabelKind::Worktree => {
                        spans.push(worktree_label(&l.text));
                        spans.push(Span::raw(" "));
                    }
                    kind => {
                        let (text, color) = match kind {
                            LabelKind::Branch => (l.text.clone(), Color::Green),
                            LabelKind::Remote => (l.text.clone(), Color::Red),
                            _ => (format!("tag: {}", l.text), Color::Yellow),
                        };
                        if !refs.is_empty() {
                            refs.push(Span::styled(", ", Style::new().fg(C_DIM)));
                        }
                        refs.push(Span::styled(text, Style::new().fg(color)));
                    }
                }
            }
            if !refs.is_empty() {
                spans.push(Span::styled("(", Style::new().fg(C_DIM)));
                spans.extend(refs);
                spans.push(Span::styled(") ", Style::new().fg(C_DIM)));
            }
            spans.push(Span::raw(c.subject.clone()));
        }
        RowKind::Uncommitted(wi) => {
            if let Some(w) = app.snap.worktrees.get(wi) {
                spans.push(worktree_label(&w.name));
                spans.push(Span::raw(" "));
                spans.extend(count_spans(w));
                spans.push(Span::styled(
                    " uncommitted",
                    Style::new().fg(C_DIM).add_modifier(Modifier::ITALIC),
                ));
            }
        }
    }
    Line::from(spans)
}

fn draw_graph(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Graph;
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

    if let Some(path) = app.graph_jump.take()
        && let Some(&row) = g.worktree_row.get(&path)
    {
        app.graph.sel = row;
        app.graph.follow = false;
        // Only scroll when the row is not comfortably visible; then put it a third from the top.
        let margin = (height / 4).min(3);
        let off = app.graph.offset;
        if row < off + margin || row + margin >= off + height {
            app.graph.offset = row.saturating_sub(height / 3);
        }
        app.graph.offset = app.graph.offset.min(g.rows.len().saturating_sub(height));
    }
    app.graph.clamp(g.rows.len());
    app.graph.scroll_into_view(height);

    let now = now_secs();
    let mut lines: Vec<Line> = g.rows[app.graph.offset..]
        .iter()
        .take(height)
        .map(|r| graph_line(app, r, now))
        .collect();
    if g.truncated && lines.len() < height {
        lines.push(Line::from(Span::styled(
            format!(" … older history not loaded yet ({} commits shown)", g.commits.len()),
            Style::new().fg(C_DIM),
        )));
    }
    let sel = app.graph.sel.checked_sub(app.graph.offset);
    paint_rows(f, inner, lines, sel, focused);
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

fn worktree_details(w: &WorktreeView) -> Line<'static> {
    let dim = Style::new().fg(C_DIM);
    let mut spans = vec![Span::raw(tilde(&w.path))];
    let sep = || Span::styled("  ·  ", dim);
    if let Some(h) = &w.head {
        spans.push(sep());
        spans.push(Span::styled(
            h[..h.len().min(8)].to_string(),
            Style::new().fg(Color::Yellow),
        ));
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
    Line::from(spans)
}

fn graph_details(app: &App) -> Option<Line<'static>> {
    let g = &app.snap.graph;
    match g.rows.get(app.graph.sel)?.kind {
        RowKind::Uncommitted(wi) => app.snap.worktrees.get(wi).map(worktree_details),
        RowKind::Commit(ci) => {
            let c = &g.commits[ci];
            let dim = Style::new().fg(C_DIM);
            let mut spans = vec![
                Span::styled(c.id[..c.id.len().min(10)].to_string(), Style::new().fg(Color::Yellow)),
                Span::styled("  ·  ", dim),
                Span::raw(c.author.clone()),
                Span::styled("  ·  ", dim),
                Span::raw(format!("{} ({} ago)", utc_date(c.time), age(c.time, now_secs()))),
            ];
            if c.parents.len() > 1 {
                spans.push(Span::styled(format!("  ·  merge of {}", c.parents.len()), dim));
            }
            Some(Line::from(spans))
        }
    }
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

fn draw_diff(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(d) = &app.diff else { return };
    let height = area.height.saturating_sub(2) as usize;
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(d.title.clone(), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ]);
    let position = match &d.lines {
        Some(Ok(lines)) if !lines.is_empty() => {
            let end = (d.scroll + height).min(lines.len());
            format!(" {}–{} of {} ", d.scroll + 1, end, lines.len())
        }
        _ => String::new(),
    };
    let block = panel(title, true).title_top(Line::styled(position, Style::new().fg(C_DIM)).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let body: Vec<Line> = match &d.lines {
        None => vec![Line::styled(" loading…", Style::new().fg(C_DIM))],
        Some(Err(e)) => e
            .lines()
            .map(|l| Line::styled(format!(" {l}"), Style::new().fg(C_CONFLICT)))
            .collect(),
        Some(Ok(lines)) => lines[d.scroll.min(lines.len())..]
            .iter()
            .take(height)
            .map(|l| {
                let visible = skip_cols(l, d.hscroll);
                stat_line(visible).unwrap_or_else(|| Line::styled(visible.to_string(), diff_line_style(l)))
            })
            .collect(),
    };
    f.render_widget(Paragraph::new(body), inner);
}

fn draw_diff_footer(f: &mut Frame, app: &App, area: Rect) {
    let [details, hints] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    if let Some(d) = &app.diff {
        let mut msg = match &d.target {
            crate::diff::Target::Worktree { path, .. } => tilde(path),
            crate::diff::Target::Commit { id, .. } => id.clone(),
        };
        if d.hscroll > 0 {
            msg.push_str(&format!("  ·  scrolled right {} columns", d.hscroll));
        }
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {msg}"), Style::new().fg(C_DIM))),
            details,
        );
    }
    let key = |k: &'static str| Span::styled(k, Style::new().fg(Color::Cyan));
    let text = |t: &'static str| Span::styled(t, Style::new().fg(C_DIM));
    let line = Line::from(vec![
        Span::raw(" "),
        key("↑↓ PgUp PgDn"),
        text(" scroll  "),
        key("n/p"),
        text(" next/prev file  "),
        key("←→"),
        text(" pan  "),
        key("r"),
        text(" reload  "),
        key("Esc"),
        text(" back"),
    ]);
    f.render_widget(Paragraph::new(line), hints);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [details, hints] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let line = match app.focus {
        Focus::Worktrees => app.snap.worktrees.get(app.wt.sel).map(worktree_details),
        Focus::Graph => graph_details(app),
    };
    if let Some(e) = &app.snap.error {
        let msg = format!(" {}", e.lines().next().unwrap_or(""));
        f.render_widget(Paragraph::new(Span::styled(msg, Style::new().fg(C_CONFLICT))), details);
    } else if let Some(mut line) = line {
        line.spans.insert(0, Span::raw(" "));
        f.render_widget(Paragraph::new(line), details);
    }

    let key = |k: &'static str| Span::styled(k, Style::new().fg(Color::Cyan));
    let text = |t: &'static str| Span::styled(t, Style::new().fg(C_DIM));
    let left = Line::from(vec![
        Span::raw(" "),
        key("↑↓"),
        text(" move  "),
        key("Tab"),
        text(" focus  "),
        key("Enter"),
        text(" diff  "),
        key("?"),
        text(" help  "),
        key("q"),
        text(" quit"),
    ]);
    let left_width = left.width();
    f.render_widget(Paragraph::new(left), hints);
    let mode = if app.snap.watching {
        format!("live · poll {:.1}s ", app.interval_ms as f64 / 1000.0)
    } else {
        format!("polling {:.1}s ", app.interval_ms as f64 / 1000.0)
    };
    if left_width + mode.width() < hints.width as usize {
        let right = Line::from(Span::styled(mode, Style::new().fg(C_DIM))).right_aligned();
        f.render_widget(Paragraph::new(right), hints);
    }
}

const HELP: &[(&str, &str)] = &[
    ("↑ ↓  j k", "move selection"),
    ("PgUp PgDn  ^u ^d", "page / half page"),
    ("g G  Home End", "first / last"),
    ("Tab  ← →  h l", "switch panel"),
    ("Enter  dbl-click", "diff of commit or worktree"),
    ("r", "refresh now"),
    ("?", "toggle this help"),
    ("q  Ctrl-C", "quit"),
    ("", ""),
    ("in diff view:", ""),
    ("↑ ↓  PgUp PgDn", "scroll"),
    ("n p  ] [", "next / previous file"),
    ("← →  h l", "scroll sideways"),
    ("r", "reload"),
    ("Esc  q", "back"),
];

fn draw_help(f: &mut Frame) {
    let area = f.area();
    let w = 52.min(area.width);
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
    let block = panel(Line::from(" keys "), true);
    f.render_widget(
        Paragraph::new(lines).block(block.padding(ratatui::widgets::Padding::vertical(1))),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_ages() {
        assert_eq!(utc_date(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_date(1700003600), "2023-11-14 23:13 UTC");
        assert_eq!(utc_date(951782400), "2000-02-29 00:00 UTC");
        assert_eq!(age(100, 100), "0s");
        assert_eq!(age(0, 7200), "2h");
        assert_eq!(age(0, 86400 * 3), "3d");
        assert_eq!(age(0, 86400 * 400), "1y");
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
