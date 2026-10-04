//! canopy: a live view of every git worktree of a repo.
//!
//! Layers: `git` (shelling out + parsers), `model` (snapshot types), `watch` (background refresh),
//! `ui` (ratatui front end, renders snapshots only).

pub mod git;
pub mod model;
pub mod ui;
pub mod watch;
