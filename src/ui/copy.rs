//! Copy shortcuts: `y` then a key copies something about the line under the cursor (or the open
//! diff) to the system clipboard. `yy` copies the obvious thing: the file on a file line, the sha
//! on a commit, the path on a worktree.

use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::inset::{Item, VLine};
use super::{App, Focus, UiMsg, spawn_named};
use crate::clipboard;
use crate::diff;
use crate::graph::{LabelKind, RowKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Sha,
    ShortSha,
    Branch,
    Subject,
    Message,
    Path,
    File,
    Diff,
}

/// The second key of each `y` chord, in the order the footer lists them.
pub const FIELDS: [(char, Field); 8] = [
    ('h', Field::Sha),
    ('s', Field::ShortSha),
    ('b', Field::Branch),
    ('m', Field::Subject),
    ('M', Field::Message),
    ('p', Field::Path),
    ('f', Field::File),
    ('d', Field::Diff),
];

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Sha => "sha",
            Field::ShortSha => "short sha",
            Field::Branch => "branch",
            Field::Subject => "subject",
            Field::Message => "message",
            Field::Path => "path",
            Field::File => "file",
            Field::Diff => "diff",
        }
    }
}

/// What the current line offers to copy.
#[derive(Clone, Debug, Default)]
pub struct CopyCtx {
    /// The commit (for a worktree: its HEAD).
    pub sha: Option<String>,
    pub subject: Option<String>,
    pub branch: Option<String>,
    /// Worktree path.
    pub path: Option<PathBuf>,
    /// File path (relative to the repository root) when the cursor is on a file.
    pub file: Option<String>,
    /// What `yd` copies (only `file` when there is one).
    pub diff: Option<diff::Target>,
    /// What `yy` copies.
    pub default: Option<Field>,
}

impl CopyCtx {
    pub fn has(&self, f: Field) -> bool {
        match f {
            Field::Sha | Field::ShortSha | Field::Message => self.sha.is_some(),
            Field::Branch => self.branch.is_some(),
            Field::Subject => self.subject.is_some(),
            Field::Path => self.path.is_some(),
            Field::File => self.file.is_some(),
            Field::Diff => self.diff.is_some(),
        }
    }
}

/// How the copied text is obtained (git runs off the UI thread).
enum Payload {
    Text(String),
    Message(PathBuf, String),
    Patch(diff::Target, Option<String>),
}

impl App {
    /// Whether `k` starts a `y` chord.
    pub(super) fn starts_copy(&self, k: KeyEvent) -> bool {
        let keys_to_shell = self.shell_focus && self.shell.is_some() && self.diff.is_none();
        k.code == KeyCode::Char('y')
            && k.modifiers.difference(KeyModifiers::SHIFT).is_empty()
            && !self.help
            && !keys_to_shell
    }

    /// The key after `y`.
    pub(super) fn on_copy_key(&mut self, k: KeyEvent) {
        let field = match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
                return;
            }
            KeyCode::Char('y') => self.copy_ctx().default,
            KeyCode::Char(c) => FIELDS.iter().find(|(key, _)| *key == c).map(|&(_, f)| f),
            _ => None,
        };
        if let Some(field) = field {
            self.copy(field);
        }
    }

    pub fn copy_ctx(&self) -> CopyCtx {
        if self.diff.is_some() {
            return self.diff_copy_ctx();
        }
        let f = self.focus;
        let p = self.panel(f);
        let Some(&line) = p.lines.get(p.pos.sel) else {
            return CopyCtx::default();
        };
        let r = line.row();
        let g = &self.snap.graph;
        let (wi, sha) = match f {
            Focus::Worktrees => (Some(r), self.snap.worktrees.get(r).and_then(|w| w.head.clone())),
            Focus::Graph => match g.rows.get(r).map(|row| row.kind) {
                Some(RowKind::Commit(ci)) => (self.graph_row_worktree(), Some(g.commits[ci].id.clone())),
                Some(RowKind::Uncommitted(wi)) => (Some(wi), self.snap.worktrees.get(wi).and_then(|w| w.head.clone())),
                None => return CopyCtx::default(),
            },
        };
        let file = match line {
            VLine::Inset(_, Item::File(fi) | Item::Diff(fi, _) | Item::More(fi, _)) => self.file_path(f, r, fi),
            _ => None,
        };
        let on_commit = f == Focus::Graph && matches!(g.rows.get(r).map(|row| row.kind), Some(RowKind::Commit(_)));
        let default = if file.is_some() {
            Field::File
        } else if on_commit {
            Field::Sha
        } else {
            Field::Path
        };
        let diff = self.target(f, r).and_then(|k| self.diff_target(&k));
        self.fill_ctx(wi, sha, file, diff, default)
    }

    fn diff_copy_ctx(&self) -> CopyCtx {
        let Some(d) = &self.diff else { return CopyCtx::default() };
        let wts = &self.snap.worktrees;
        let (wi, sha, default) = match &d.target {
            diff::Target::Commit { id, .. } => (
                wts.iter().position(|w| w.head.as_ref() == Some(id)),
                Some(id.clone()),
                Field::Sha,
            ),
            diff::Target::Worktree { path, .. } | diff::Target::Branch { path, .. } => {
                let wi = wts.iter().position(|w| &w.path == path);
                (wi, wi.and_then(|wi| wts[wi].head.clone()), Field::Path)
            }
        };
        let file = d.selected_file().map(|f| f.path.clone());
        let default = if file.is_some() { Field::File } else { default };
        self.fill_ctx(wi, sha, file, Some(d.target.clone()), default)
    }

    fn fill_ctx(
        &self,
        wi: Option<usize>,
        sha: Option<String>,
        file: Option<String>,
        diff: Option<diff::Target>,
        default: Field,
    ) -> CopyCtx {
        let g = &self.snap.graph;
        let ci = sha.as_ref().and_then(|id| match g.rows[*g.commit_row.get(id)?].kind {
            RowKind::Commit(ci) => Some(ci),
            RowKind::Uncommitted(_) => None,
        });
        let w = wi.and_then(|wi| self.snap.worktrees.get(wi));
        // The worktree's branch, else the first local branch pointing at the commit.
        let branch = w.and_then(|w| w.branch.clone()).or_else(|| {
            let labels = g.labels.get(ci?)?;
            labels
                .iter()
                .find(|l| l.kind == LabelKind::Branch)
                .map(|l| l.text.clone())
        });
        CopyCtx {
            subject: ci.map(|ci| g.commits[ci].subject.clone()),
            sha,
            branch,
            path: w.map(|w| w.path.clone()),
            file,
            diff,
            default: Some(default),
        }
    }

    /// Copy `field` of the current line in the background; the footer reports the outcome.
    fn copy(&mut self, field: Field) {
        let ctx = self.copy_ctx();
        if !ctx.has(field) {
            self.notice = Some(format!("no {} to copy here", field.label()));
            return;
        }
        let sha = ctx.sha.clone().unwrap_or_default();
        let (what, payload) = match field {
            Field::Sha => ("sha".to_string(), Payload::Text(sha)),
            Field::ShortSha => (
                "short sha".to_string(),
                Payload::Text(sha[..sha.len().min(7)].to_string()),
            ),
            Field::Branch => ("branch".to_string(), Payload::Text(ctx.branch.unwrap_or_default())),
            Field::Subject => ("subject".to_string(), Payload::Text(ctx.subject.unwrap_or_default())),
            Field::Message => (
                "message".to_string(),
                Payload::Message(self.snap.common_dir.clone(), sha),
            ),
            Field::Path => {
                let path = ctx.path.unwrap_or_default().to_string_lossy().into_owned();
                ("path".to_string(), Payload::Text(path))
            }
            Field::File => ("file path".to_string(), Payload::Text(ctx.file.unwrap_or_default())),
            Field::Diff => {
                let Some(target) = ctx.diff else { return };
                let what = match &ctx.file {
                    Some(f) => format!("diff of {f}"),
                    None => "diff".to_string(),
                };
                (what, Payload::Patch(target, ctx.file))
            }
        };
        let (tx, methods) = (self.tx.clone(), self.clipboard.clone());
        spawn_named("canopy-copy", move || {
            let text = match payload {
                Payload::Text(t) => Ok(t),
                Payload::Message(dir, sha) => crate::git::run(&dir, &["log", "-1", "--format=%B", &sha, "--"])
                    .map(|out| String::from_utf8_lossy(&out).trim_end().to_string())
                    .map_err(|e| format!("{e:#}")),
                Payload::Patch(target, file) => diff::patch(&target, file.as_deref()).map_err(|e| format!("{e:#}")),
            };
            let msg = match text {
                Err(e) => UiMsg::Copied {
                    notice: format!("copy failed: {e}"),
                    osc52: None,
                },
                Ok(t) if t.is_empty() => UiMsg::Copied {
                    notice: format!("{what} is empty, nothing copied"),
                    osc52: None,
                },
                Ok(t) => {
                    let done = methods.iter().any(|m| clipboard::run(m, &t));
                    UiMsg::Copied {
                        notice: describe(&what, &t),
                        osc52: (!done).then(|| clipboard::osc52(&t)),
                    }
                }
            };
            let _ = tx.send(msg);
        });
    }
}

/// `copied sha: 35d6da7…` for one line, `copied diff (120 lines)` for more. Long paths keep
/// their end, everything else its start.
fn describe(what: &str, text: &str) -> String {
    let n = text.lines().count();
    if n > 1 {
        return format!("copied {what} ({n} lines)");
    }
    const MAX: usize = 70;
    let len = text.chars().count();
    if len <= MAX {
        format!("copied {what}: {text}")
    } else if what.ends_with("path") {
        let tail: String = text.chars().skip(len - MAX).collect();
        format!("copied {what}: …{tail}")
    } else {
        let head: String = text.chars().take(MAX).collect();
        format!("copied {what}: {head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_what_was_copied() {
        assert_eq!(describe("sha", "abc"), "copied sha: abc");
        assert_eq!(describe("diff", "a\nb\nc\n"), "copied diff (3 lines)");
        assert!(describe("subject", &"x".repeat(100)).ends_with("x…"));
        let long = format!("/{}/wt/feat-auth", "d".repeat(100));
        assert!(describe("path", &long).starts_with("copied path: …ddd"));
        assert!(describe("path", &long).ends_with("/wt/feat-auth"));
    }
}
