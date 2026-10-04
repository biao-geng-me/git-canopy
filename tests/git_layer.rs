mod common;

use canopy::git::{self, log, refs, status, worktree};

#[test]
fn reads_fixture_state() {
    let fx = common::Fixture::new();
    let paths = git::discover(&fx.wt("feat-auth")).unwrap();
    assert_eq!(paths.common_dir, git::canonical(&fx.repo().join(".git")));
    assert_eq!(paths.toplevel, Some(git::canonical(&fx.wt("feat-auth"))));

    let (wts, _) = worktree::list(&fx.repo()).unwrap();
    let names: Vec<_> = wts.iter().map(canopy::model::display_name).collect();
    assert_eq!(
        names,
        [
            "main",
            "conflict",
            "detached",
            "feat-auth",
            "fix-parser",
            "gone",
            "refactor-db",
            "docs"
        ]
    );
    let by = |n: &str| wts.iter().find(|w| canopy::model::display_name(w) == n).unwrap();
    assert!(by("detached").detached);
    assert!(by("gone").prunable.is_some());
    assert!(by("refactor-db").locked.is_some());

    let st = |n: &str| status::load(&by(n).path).unwrap();
    let auth = st("feat-auth");
    assert_eq!(
        (auth.staged, auth.modified, auth.untracked, auth.unmerged),
        (3, 2, 1, 0)
    );
    let main = st("main");
    assert!(!main.is_dirty());
    assert_eq!(main.upstream.as_deref(), Some("origin/main"));
    assert_eq!(main.upstream_ab, Some((0, 2)));
    assert_eq!(st("fix-parser").modified, 1);
    assert_eq!(st("docs").untracked, 2);
    assert_eq!(st("conflict").unmerged, 1);
    assert!(!st("refactor-db").is_dirty());
    assert!(status::load(&by("gone").path).is_err());

    let admin = worktree::admin_dir(&by("conflict").path).unwrap();
    assert_eq!(worktree::operation(&admin), Some("MERGING"));
    assert_eq!(worktree::operation(&worktree::admin_dir(&fx.repo()).unwrap()), None);

    let (rs, _) = refs::load(&fx.repo()).unwrap();
    let tag = rs.iter().find(|r| r.name == "v0.1").unwrap();
    assert_eq!(tag.kind, refs::RefKind::Tag);
    assert!(rs.iter().any(|r| r.name == "origin/main"));

    let detached_head = by("detached").head.clone().unwrap();
    let commits = log::load(&fx.repo(), 1000, std::slice::from_ref(&detached_head)).unwrap();
    assert!(commits.iter().any(|c| c.id == detached_head));
    assert!(
        commits
            .iter()
            .any(|c| c.subject == "Merge branch 'feature-x'" && c.parents.len() == 2)
    );
    // Topological: every parent appears after its child.
    let pos: std::collections::HashMap<_, _> = commits.iter().enumerate().map(|(i, c)| (c.id.clone(), i)).collect();
    for (i, c) in commits.iter().enumerate() {
        for p in &c.parents {
            assert!(pos[p] > i);
        }
    }

    let refactor = by("refactor-db").head.clone().unwrap();
    assert_eq!(git::left_right_count(&fx.repo(), "main", &refactor).unwrap(), (0, 4));
}
