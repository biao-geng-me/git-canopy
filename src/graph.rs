//! Commit graph layout (lanes). Filled in by the graph milestone.

use crate::git::{log::Commit, refs::RefEntry};
use crate::model::WorktreeView;

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub truncated: bool,
}

impl Graph {
    pub fn build(
        _commits: &[Commit],
        _refs: &[RefEntry],
        _worktrees: &[WorktreeView],
        truncated: bool,
    ) -> Graph {
        Graph { truncated }
    }
}
