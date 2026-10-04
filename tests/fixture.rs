mod common;

use std::process::Command;

#[test]
fn fixture_has_expected_worktrees() {
    let fx = common::Fixture::new();
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(fx.repo())
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.matches("worktree ").count(), 8, "{text}");
    assert!(fx.wt("feat-auth").exists());
}
