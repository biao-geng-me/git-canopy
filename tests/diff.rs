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

    let doc = diff::load(&Target::Commit {
        dir: common_dir,
        id: head("refactor-db"),
    })
    .unwrap();
    assert!(doc.overview[0].starts_with("commit "));
    assert!(doc.overview.iter().any(|l| l.contains("db: refactor step 4")));
    assert_eq!(doc.files.len(), 1);
    assert_eq!(
        (doc.files[0].path.as_str(), doc.files[0].status, doc.files[0].adds),
        ("src/db.rs", 'M', 1)
    );
    assert!(doc.files[0].lines.iter().any(|l| l == "+// step 4"));

    let doc = diff::load(&Target::Worktree {
        path: fx.wt("feat-auth"),
        has_head: true,
    })
    .unwrap();
    let files: Vec<_> = doc.files.iter().map(|f| (f.path.as_str(), f.status)).collect();
    assert_eq!(
        files,
        [
            ("src/auth.rs", 'M'),
            ("src/lib.rs", 'M'),
            ("src/main.rs", 'M'),
            ("src/session.rs", 'A'),
            ("src/token.rs", 'A'),
            ("notes.md", '?'),
        ]
    );
    assert!(doc.files[0].lines.iter().any(|l| l == "+// wip"), "unstaged change");
    assert!(
        doc.files[4].lines.iter().any(|l| l == "+pub struct Token;"),
        "staged new file"
    );
    assert!(
        doc.files[5].lines.iter().any(|l| l == "+scratch"),
        "untracked file content"
    );
    assert!(doc.overview.iter().any(|l| l == "Untracked files (1):"));

    let doc = diff::load(&Target::Worktree {
        path: fx.wt("untracked-only"),
        has_head: true,
    })
    .unwrap();
    let files: Vec<_> = doc.files.iter().map(|f| (f.path.as_str(), f.adds)).collect();
    assert_eq!(files, [("TODO", 1), ("docs/guide.md", 3)]);

    let doc = diff::load(&Target::Worktree {
        path: fx.wt("refactor-db"),
        has_head: true,
    })
    .unwrap();
    assert_eq!(doc.overview, ["No uncommitted changes."]);
    assert!(doc.files.is_empty());
}
