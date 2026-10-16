// Embeds what was built into CANOPY_BUILD ("0.3.0 · 00faf25 · 2026-10-15 · x86_64-unknown-linux-musl"),
// shown by `--version` and the `?` popup. The commit comes from git, or from CANOPY_BUILD_REV
// ("<sha> <date>") when building an export without .git (scripts/build-x86.sh); neither → left out.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=CANOPY_BUILD_REV");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let rev = match std::env::var("CANOPY_BUILD_REV") {
        Ok(r) if !r.trim().is_empty() => Some(r.trim().replace(' ', " · ")),
        _ => git(&["log", "-1", "--format=%h %cs"]).filter(|r| !r.is_empty()).map(|r| {
            // Rerun when HEAD moves or the index changes (commit, checkout, staging).
            for p in ["HEAD", "index", "packed-refs"] {
                if let Some(p) = git(&["rev-parse", "--git-path", p]) {
                    println!("cargo:rerun-if-changed={p}");
                }
            }
            if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r])) {
                println!("cargo:rerun-if-changed={r}");
            }
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            let (sha, date) = r.split_once(' ').unwrap_or((&r, ""));
            format!("{sha}{} · {date}", if dirty { "-dirty" } else { "" })
        }),
    };
    let mut build = env!("CARGO_PKG_VERSION").to_string();
    if let Some(rev) = rev {
        build.push_str(&format!(" · {rev}"));
    }
    build.push_str(&format!(" · {}", std::env::var("TARGET").unwrap_or_default()));
    println!("cargo:rustc-env=CANOPY_BUILD={build}");
}
