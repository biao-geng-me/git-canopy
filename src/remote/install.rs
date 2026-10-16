//! `canopy remote install <host>`: put this canopy (or a given binary) on a remote host, where
//! `HOST:PATH` runs it as `canopy serve`. Over ssh like everything remote; nothing else needed
//! there.

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::client::{self, shell_quote};

/// Where it goes by default (the XDG user bin directory).
pub const DEFAULT_DIR: &str = "~/.local/bin";

pub struct Install {
    pub host: String,
    /// Target directory there; `~/` means its home.
    pub dir: String,
    /// The binary to copy; this canopy when `None` (then the host must have this machine's
    /// architecture).
    pub binary: Option<PathBuf>,
}

/// What `ssh <host> <script>` printed (stdout), failing with its stderr.
fn remote(host: &str, script: &str, input: Option<&Path>) -> Result<String> {
    let ssh = std::env::var_os("CANOPY_SSH").unwrap_or_else(|| "ssh".into());
    let mut cmd: Command = client::ssh(&ssh, host, script, &[]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().context("running ssh")?;
    if let Some(path) = input {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(&bytes).context("sending the binary")?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("on {host}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `dir` for the remote shell: `~/x` as `"$HOME"/'x'`, anything else quoted.
fn shell_dir(dir: &str) -> String {
    match dir.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None if dir == "~" => "\"$HOME\"".into(),
        None => shell_quote(dir),
    }
}

/// Install, printing what it does; returns the installed path there.
pub fn run(opts: &Install) -> Result<String> {
    let host = &opts.host;
    let d = shell_dir(&opts.dir);
    // Probe: architecture, the directory (made if missing) as a physical path with its inode
    // and birth time, and whether `canopy` there is a symlink (a development setup).
    let probe = format!(
        "uname -m; mkdir -p {d} && cd {d} && pwd -P && stat -c '%i %W' . && \
         if [ -L canopy ]; then echo symlink; else echo file; fi"
    );
    let out = remote(host, &probe, None)?;
    let lines: Vec<&str> = out.lines().collect();
    let [arch, dir, stat, kind] = lines[..] else {
        bail!("unexpected answer from {host}: {out}");
    };
    let here = std::env::consts::ARCH;
    eprintln!("{host}: {arch}, installing into {dir}");
    if kind == "symlink" {
        bail!("{dir}/canopy on {host} is a symlink (a development setup?); not replacing it");
    }
    // The same directory seen from here (a shared home)? Then a binary for another
    // architecture would break this machine's canopy.
    let shared = std::fs::metadata(dir).ok().is_some_and(|m| {
        let birth = m
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        stat == format!("{} {birth}", m.ino())
    });
    let binary = match &opts.binary {
        Some(b) => b.clone(),
        None if arch == here => std::env::current_exe().context("finding this canopy")?,
        None => bail!(
            "{host} is {arch}, this machine {here}: give a canopy built for {arch} with --binary \
             (the releases have a static x86_64 build)"
        ),
    };
    if shared && arch != here {
        bail!(
            "{dir} on {host} is the same directory as here (a shared home), and {host} is {arch} \
             but this machine {here}: install into a per-architecture directory with --dir"
        );
    }
    if shared && opts.binary.is_none() && same_file(&binary, &Path::new(dir).join("canopy")) {
        eprintln!("{dir}/canopy is this canopy already (a shared directory)");
    } else {
        // Into a temporary name, then renamed: a canopy running there keeps its old file.
        let upload = format!("cd {d} && t=.canopy.$$ && cat > \"$t\" && chmod 755 \"$t\" && mv -f \"$t\" canopy");
        let size = std::fs::metadata(&binary).map(|m| m.len()).unwrap_or(0);
        eprintln!("copying {} ({} KiB)", binary.display(), size / 1024);
        remote(host, &upload, Some(&binary))?;
    }
    let installed = format!("{dir}/canopy");
    let check = format!("{} --version; command -v canopy || true", shell_quote(&installed));
    let out = remote(host, &check, None).context("the installed canopy doesn't run there")?;
    let mut lines = out.lines();
    let version = lines.next().unwrap_or_default();
    let on_path = lines.next().unwrap_or_default();
    eprintln!("{host}: {version}");
    if on_path != installed {
        let found = if on_path.is_empty() {
            "isn't on its PATH".to_string()
        } else {
            format!("resolves to {on_path}")
        };
        eprintln!(
            "note: on {host}, `canopy` {found} for ssh commands; use \
             --remote-cmd {installed} (or CANOPY_REMOTE_CMD)"
        );
    }
    Ok(installed)
}

fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(a), Ok(b)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    a.ino() == b.ino() && a.dev() == b.dev()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_dirs() {
        assert_eq!(shell_dir("~/.local/bin"), "\"$HOME\"/.local/bin");
        assert_eq!(shell_dir("~/my bin"), "\"$HOME\"/'my bin'");
        assert_eq!(shell_dir("/opt/x"), "/opt/x");
        assert_eq!(shell_dir("~"), "\"$HOME\"");
    }
}
