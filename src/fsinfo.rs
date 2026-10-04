//! What kind of filesystem a path lives on, to decide whether file system events can be trusted.

use std::path::{Path, PathBuf};

/// How far inotify-style events can be trusted on a filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Events {
    /// Local filesystem: every change is reported.
    Reliable,
    /// Network filesystem (NFS, Lustre, GPFS, …): only changes made from this machine are
    /// reported; changes from other nodes are invisible.
    LocalOnly,
    /// Events don't arrive at all (e.g. WSL2's `/mnt/c` over 9p, most FUSE mounts).
    Unreliable,
}

/// Classify a filesystem type name from `/proc/self/mountinfo`.
pub fn events_for(fstype: &str) -> Events {
    match fstype {
        "9p" | "drvfs" | "virtiofs" | "fuseblk" => Events::Unreliable,
        t if t.starts_with("fuse") => Events::Unreliable,
        "nfs" | "nfs4" | "lustre" | "gpfs" | "beegfs" | "cifs" | "smb3" | "smbfs" | "ceph" | "panfs" | "afs"
        | "glusterfs" | "pvfs2" | "orangefs" => Events::LocalOnly,
        _ => Events::Reliable,
    }
}

/// Filesystem type of the mount containing `path` (Linux only; `None` elsewhere or on error).
pub fn fs_type(path: &Path) -> Option<String> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    fs_type_in(&info, &crate::git::canonical(path))
}

pub fn events_at(path: &Path) -> Events {
    fs_type(path).map_or(Events::Reliable, |t| events_for(&t))
}

/// Find the deepest mount point containing `path` in mountinfo text and return its fs type.
fn fs_type_in(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        // id parent major:minor root mountpoint options [optional...] - fstype source superopts
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let (Some(mount), Some(fstype)) = (pre.split(' ').nth(4), post.split(' ').next()) else {
            continue;
        };
        let mount = unescape(mount);
        let depth = mount.components().count();
        if path.starts_with(&mount) && best.as_ref().is_none_or(|(d, _)| depth >= *d) {
            best = Some((depth, fstype.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

/// mountinfo escapes space, tab, newline and backslash as octal (`\040`).
fn unescape(s: &str) -> PathBuf {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let code: String = chars.by_ref().take(3).collect();
            match u8::from_str_radix(&code, 8) {
                Ok(b) => out.push(b as char),
                Err(_) => {
                    out.push('\\');
                    out.push_str(&code);
                }
            }
        } else {
            out.push(c);
        }
    }
    PathBuf::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
80 65 8:48 / / rw,relatime - ext4 /dev/sdd rw
91 80 0:55 / /mnt/c rw,noatime - 9p C:\\134 rw,aname=drvfs
92 80 0:56 / /scratch rw - lustre 10.0.0.1@o2ib:/fs rw
93 92 0:57 / /scratch/my\\040dir rw - nfs4 server:/x rw";

    #[test]
    fn finds_deepest_mount() {
        let t = |p: &str| fs_type_in(SAMPLE, Path::new(p));
        assert_eq!(t("/home/u/repo").as_deref(), Some("ext4"));
        assert_eq!(t("/mnt/c/Users/x").as_deref(), Some("9p"));
        assert_eq!(t("/scratch/proj").as_deref(), Some("lustre"));
        assert_eq!(t("/scratch/my dir/a").as_deref(), Some("nfs4"));
        assert_eq!(t("/mnt/cc").as_deref(), Some("ext4"), "component-wise prefix");
    }

    #[test]
    fn classifies() {
        assert_eq!(events_for("ext4"), Events::Reliable);
        assert_eq!(events_for("9p"), Events::Unreliable);
        assert_eq!(events_for("fuse.sshfs"), Events::Unreliable);
        assert_eq!(events_for("lustre"), Events::LocalOnly);
        assert_eq!(events_for("nfs4"), Events::LocalOnly);
    }
}
