//! Test helpers: build the fixture repo from scripts/mkfixture.sh in a fresh temp dir.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Fixture {
    pub dir: PathBuf,
}

impl Fixture {
    pub fn new() -> Fixture {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "canopy-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/mkfixture.sh");
        let out = Command::new("bash")
            .arg(script)
            .arg(&dir)
            .output()
            .expect("run mkfixture.sh");
        assert!(
            out.status.success(),
            "mkfixture failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Fixture { dir }
    }

    pub fn repo(&self) -> PathBuf {
        self.dir.join("repo")
    }

    pub fn wt(&self, name: &str) -> PathBuf {
        self.dir.join("wt").join(name)
    }

    /// Make git's last use of the linked worktree at `path` two hours old, past the time a
    /// worktree without commits of its own is held back from cleanup.
    #[allow(dead_code)]
    pub fn age(&self, path: &std::path::Path) {
        let admin = canopy::git::worktree::admin_dir(path).expect("admin dir");
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        for f in ["HEAD", "index", "commondir"] {
            if let Ok(file) = std::fs::File::options().append(true).open(admin.join(f)) {
                file.set_modified(then).unwrap();
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
