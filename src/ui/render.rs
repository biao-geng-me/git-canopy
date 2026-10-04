//! Drawing. Pure functions of [`App`] state (plus scroll-offset bookkeeping).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::{App, Focus, ListPos};
use crate::model::WorktreeView;

pub const C_STAGED: Color = Color::Green;
pub const C_MODIFIED: Color = Color::Yellow;
pub const C_UNTRACKED: Color = Color::Magenta;
pub const C_CONFLICT: Color = Color::Red;
pub const C_DIM: Color = Color::DarkGray;
const SEL_FOCUSED: Color = Color::Indexed(238);
const SEL_UNFOCUSED: Color = Color::Indexed(235);

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(f.area());
    let wt_width = worktree_panel_width(app, main.width);
    let [left, right] =
        Layout::horizontal([Constraint::Length(wt_width), Constraint::Min(10)]).areas(main);
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
fn paint_rows(
    f: &mut Frame,
    inner: Rect,
    lines: Vec<Line<'static>>,
    sel: Option<usize>,
    focused: bool,
) {
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

fn worktree_panel_width(app: &App, total: u16) -> u16 {
    let rows = wt_rows(app);
    let name = rows
        .iter()
        .map(|r| r.name.content.width())
        .max()
        .unwrap_or(4)
        .min(NAME_MAX);
    let counts = rows
        .iter()
        .map(|r| spans_width(&r.counts))
        .max()
        .unwrap_or(0);
    let ab = rows.iter().map(|r| spans_width(&r.ab)).max().unwrap_or(0);
    // borders + marker + gaps
    let natural = 2 + 2 + name + 1 + counts + if ab > 0 { 1 + ab } else { 0 } + 1;
    let max = (total as usize * 45 / 100).max(20);
    natural.clamp(24, max) as u16
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
    if w >= width {
        s
    } else {
        s + &" ".repeat(width - w)
    }
}

fn draw_worktrees(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Worktrees;
    let n = app.snap.worktrees.len();
    let dirty = app.snap.worktrees.iter().filter(|w| w.is_dirty()).count();
    let mut title = vec![Span::raw(" worktrees ")];
    if n > 0 {
        title.push(Span::styled(format!("{n} "), Style::new().fg(C_DIM)));
        if dirty > 0 {
            title.push(Span::styled(
                format!("✎{dirty} "),
                Style::new().fg(C_MODIFIED),
            ));
        }
    }
    let block = panel(Line::from(title), focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if n == 0 {
        f.render_widget(
            Paragraph::new(Span::styled(" loading…", Style::new().fg(C_DIM))),
            inner,
        );
        return;
    }

    let rows = wt_rows(app);
    let avail = inner.width as usize;
    let counts_w = rows
        .iter()
        .map(|r| spans_width(&r.counts))
        .max()
        .unwrap_or(0);
    let ab_w = rows.iter().map(|r| spans_width(&r.ab)).max().unwrap_or(0);
    let name_natural = rows
        .iter()
        .map(|r| r.name.content.width())
        .max()
        .unwrap_or(0);
    let fixed = 2 + 1 + counts_w + if ab_w > 0 { 1 + ab_w } else { 0 };
    let name_w = name_natural
        .min(NAME_MAX)
        .min(avail.saturating_sub(fixed).max(6));

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

fn draw_graph(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Graph;
    let block = panel(Line::from(" graph "), focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let _ = (&app.graph, ListPos::default());
    f.render_widget(
        Paragraph::new(Span::styled(
            " (graph not implemented yet)",
            Style::new().fg(C_DIM),
        )),
        inner,
    );
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
    match (
        &w.branch,
        w.status.as_ref().and_then(|s| s.upstream.clone()),
    ) {
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
        let r = if r.is_empty() {
            String::new()
        } else {
            format!(": {r}")
        };
        spans.push(Span::styled(
            format!("locked{r}"),
            Style::new().fg(C_MODIFIED),
        ));
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

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [details, hints] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let line = match app.focus {
        Focus::Worktrees | Focus::Graph => app.snap.worktrees.get(app.wt.sel).map(worktree_details),
    };
    if let Some(e) = &app.snap.error {
        let msg = format!(" {}", e.lines().next().unwrap_or(""));
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::new().fg(C_CONFLICT))),
            details,
        );
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
    f.render_widget(Paragraph::new(left), hints);
    let mode = if app.snap.watching {
        format!("live · poll {:.1}s ", app.interval_ms as f64 / 1000.0)
    } else {
        format!("polling {:.1}s ", app.interval_ms as f64 / 1000.0)
    };
    let right = Line::from(Span::styled(mode, Style::new().fg(C_DIM))).right_aligned();
    f.render_widget(Paragraph::new(right), hints);
}

const HELP: &[(&str, &str)] = &[
    ("↑ ↓  j k", "move selection"),
    ("PgUp PgDn  ^u ^d", "page / half page"),
    ("g G  Home End", "first / last"),
    ("Tab  ← →  h l", "switch panel"),
    ("Enter", "diff of commit or worktree"),
    ("r", "refresh now"),
    ("?", "toggle this help"),
    ("q  Ctrl-C", "quit"),
];

fn draw_help(f: &mut Frame) {
    let area = f.area();
    let w = 50.min(area.width);
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
                Span::styled(format!(" {k:<18}"), Style::new().fg(Color::Cyan)),
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
