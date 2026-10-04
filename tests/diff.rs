mod common;

use canopy::diff::{self, Target};
use canopy::git;

#[test]
fn commit_and_worktree_diffs() {
    let fx = common::Fixture::new();
    let common_dir = git::discover(&fx.repo()).unwrap().common_dir;
    let (wts, _) = git::worktree::list(&fx.repo()).unwrap();
    let head = |name: &str| {
        wts.iter()
            .find(|w| w.path.ends_with(name))
            .and_then(|w| w.head.clone())
            .unwrap()
    };

    let lines = diff::load(&Target::Commit {
        dir: common_dir,
        id: head("refactor-db"),
    })
    .unwrap();
    assert!(lines[0].starts_with("commit "));
    assert!(lines.iter().any(|l| l.contains("db: refactor step 4")));
    assert!(lines.iter().any(|l| l == "+// step 4"));

    let lines = diff::load(&Target::Worktree {
        path: fx.wt("feat-auth"),
        has_head: true,
    })
    .unwrap();
    assert!(lines.iter().any(|l| l == "+// wip"), "unstaged change");
    assert!(lines.iter().any(|l| l == "+pub struct Token;"), "staged new file");
    assert!(lines.iter().any(|l| l == "Untracked files (1):"));
    assert!(lines.iter().any(|l| l == "+scratch"), "untracked file content");

    let lines = diff::load(&Target::Worktree {
        path: fx.wt("untracked-only"),
        has_head: true,
    })
    .unwrap();
    assert!(lines.iter().any(|l| l == "Untracked files (2):"));
    assert!(lines.iter().any(|l| l == "+# Guide"));

    let lines = diff::load(&Target::Worktree {
        path: fx.wt("refactor-db"),
        has_head: true,
    })
    .unwrap();
    assert_eq!(lines, ["No uncommitted changes."]);
}
