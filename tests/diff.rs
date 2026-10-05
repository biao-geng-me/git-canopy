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

/// `git apply -R --check` of `patch` in `wt`: the patch describes exactly what is there.
fn reverse_applies(wt: &std::path::Path, patch: &str) -> bool {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["apply", "-R", "--check", "-"])
        .current_dir(wt)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(patch.as_bytes()).unwrap();
    child.wait().unwrap().success()
}

#[test]
fn copied_patches_are_real_patches() {
    let fx = common::Fixture::new();
    let wt = fx.wt("feat-auth");
    // A CRLF untracked file without a final newline must survive too.
    std::fs::write(wt.join("crlf.txt"), "one\r\ntwo").unwrap();
    let target = Target::Worktree {
        path: wt.clone(),
        has_head: true,
    };

    let all = diff::patch(&target, None).unwrap();
    for f in ["src/auth.rs", "src/session.rs", "notes.md", "crlf.txt"] {
        assert!(all.contains(&format!("diff --git a/{f} b/{f}")), "{f} in\n{all}");
    }
    assert!(all.contains("+two\n\\ No newline at end of file\n"));
    assert!(reverse_applies(&wt, &all), "whole worktree patch:\n{all}");

    // One file: tracked, or untracked.
    let one = diff::patch(&target, Some("src/auth.rs")).unwrap();
    assert_eq!(one.matches("diff --git").count(), 1);
    assert!(reverse_applies(&wt, &one));
    let one = diff::patch(&target, Some("notes.md")).unwrap();
    assert!(one.starts_with("diff --git a/notes.md b/notes.md\nnew file mode 100644\n"));
    assert!(reverse_applies(&wt, &one));

    // A commit: `git show` with its header; one file without it.
    let common_dir = git::discover(&fx.repo()).unwrap().common_dir;
    let (wts, _) = git::worktree::list(&fx.repo()).unwrap();
    let id = wts
        .iter()
        .find(|w| w.path.ends_with("refactor-db"))
        .and_then(|w| w.head.clone())
        .unwrap();
    let target = Target::Commit {
        dir: common_dir,
        id: id.clone(),
    };
    let all = diff::patch(&target, None).unwrap();
    assert!(all.starts_with(&format!("commit {id}")));
    assert!(all.contains("+// step 4"));
    let one = diff::patch(&target, Some("src/db.rs")).unwrap();
    assert!(one.starts_with("diff --git a/src/db.rs b/src/db.rs"), "{one}");
}

#[test]
fn branch_diff_covers_commits_and_uncommitted_changes() {
    let fx = common::Fixture::new();
    let branch = |name: &str| Target::Branch {
        path: fx.wt(name),
        base: "main".into(),
    };
    let doc = diff::load(&branch("feat-auth")).unwrap();
    assert!(doc.overview[0].starts_with("Everything changed since branching from main at "));
    let files: Vec<_> = doc.files.iter().map(|f| (f.path.as_str(), f.status, f.adds)).collect();
    assert_eq!(
        files,
        [
            // Two commits plus an unstaged line.
            ("src/auth.rs", 'A', 3),
            ("src/lib.rs", 'M', 1),
            ("src/main.rs", 'M', 1),
            ("src/session.rs", 'A', 1),
            ("src/token.rs", 'A', 1),
            ("notes.md", '?', 1),
        ]
    );

    // Clean and at main: nothing since the fork.
    let doc = diff::load(&branch("untracked-only")).unwrap();
    assert!(doc.files.iter().all(|f| f.status == '?'));
    let doc = diff::load(&Target::Branch {
        path: fx.repo(),
        base: "main".into(),
    })
    .unwrap();
    assert!(doc.files.is_empty());
    assert!(
        doc.overview
            .iter()
            .any(|l| l == "No changes since branching from main.")
    );

    // The copyable patch for the same target applies the same files.
    let patch = diff::patch(&branch("fix-parser"), None).unwrap();
    assert!(
        patch.contains("+fn parse() {}") && patch.contains("+// fixme"),
        "{patch}"
    );
}
