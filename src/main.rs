use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use canopy::ui::LayoutMode;
use canopy::watch::Config;

const USAGE: &str = "\
canopy - live view of every git worktree of a repo

USAGE:
    canopy [OPTIONS] [PATH | HOST:PATH...]
    git canopy [OPTIONS] [PATH | HOST:PATH...]
    canopy serve [OPTIONS] [PATH...]
    canopy remote install HOST [--dir DIR] [--binary FILE]

ARGS:
    PATH    any directory inside a repository (default: current directory); several watch
            several repositories, listed in that order
    HOST:PATH
            a repository on another machine, watched there by `canopy serve` over ssh (HOST
            as ssh takes it; PATH relative to the home directory there unless absolute)

    serve   watch the repositories for a canopy on another machine, which runs this over
            ssh and talks to it on stdin/stdout (-i, --no-poll, --base, --no-watch and
            --no-herdr apply)

    remote install HOST
            copy this canopy (or --binary FILE, for another architecture) to HOST over ssh,
            into --dir DIR there (default: ~/.local/bin); refuses a directory shared with this
            machine when the architectures differ, and a symlink

OPTIONS:
    -i, --interval <SECS>   poll interval in seconds for worktrees that can't be watched (default:
                            60); watched worktrees are re-checked every 10 minutes regardless
        --no-poll           never poll; update only on file events and manual refresh (r)
        --base <BRANCH>     branch to compare worktrees against (default: main/master/...)
        --layout <MODE>     stacked (graph above worktrees, default) or side (side by side)
        --no-watch          disable filesystem notifications, poll only
        --no-herdr          don't show herdr agent activity
        --herdr-repos       also watch every repository a herdr agent or workspace is in, added
                            and removed as they come and go (PATH is optional then); on remote
                            hosts too, through each one's herdr
        --host <HOST>       connect to HOST even without a HOST:PATH (for --herdr-repos there)
        --git-tui <CMD>     git TUI that t opens in a worktree (default: $CANOPY_GIT_TUI, else
                            the first of lazygit, gitui, tig on PATH)
        --nerd-font         Nerd Font icons for untracked files and folders (default:
                            $CANOPY_NERD_FONT=1)
        --remote-cmd <CMD>  how to run canopy on a remote host, as a shell command line there
                            (default: $CANOPY_REMOTE_CMD, else canopy), e.g.
                            'XDG_CONFIG_HOME=/tmp/$USER/xdg/$(uname -m) ~/bin/canopy'
    -h, --help              print help
    -V, --version           print version

KEYS:
    ↑↓/jk move · Space expand · Enter full diff · b diff vs base · o overlaps · t git TUI · s shell · y copy · ? help
";

struct Args {
    /// `canopy serve`.
    serve: bool,
    paths: Vec<PathBuf>,
    /// `HOST:PATH` arguments.
    remote: Vec<(String, String)>,
    /// `--host` (no paths of their own).
    hosts: Vec<String>,
    cfg: Config,
    layout: LayoutMode,
    herdr: bool,
    herdr_repos: bool,
    git_tui: Option<String>,
    nerd_font: bool,
    remote_cmd: String,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut cfg = Config::default();
    let mut layout = LayoutMode::default();
    let mut herdr = true;
    let mut herdr_repos = false;
    let mut hosts: Vec<String> = Vec::new();
    let mut git_tui = std::env::var("CANOPY_GIT_TUI").ok().filter(|s| !s.trim().is_empty());
    let mut nerd_font = std::env::var("CANOPY_NERD_FONT").is_ok_and(|v| v == "1");
    let mut remote_cmd = std::env::var("CANOPY_REMOTE_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "canopy".into());
    let mut it = std::env::args_os().skip(1).peekable();
    let serve = it.next_if(|a| a == "serve").is_some();
    while let Some(arg) = it.next() {
        let s = arg.to_string_lossy().into_owned();
        if s == "--" {
            paths.extend(it.by_ref().map(PathBuf::from));
            break;
        }
        let (flag, inline) = match s.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (s.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline
                .clone()
                .or_else(|| it.next().map(|v| v.to_string_lossy().into_owned()))
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("canopy {}", env!("CANOPY_BUILD"));
                return Ok(None);
            }
            "-i" | "--interval" => {
                let v = value("--interval")?;
                let secs: f64 = v.parse().map_err(|_| format!("invalid interval: {v}"))?;
                if !(0.1..=3600.0).contains(&secs) {
                    return Err(format!("interval must be between 0.1 and 3600 seconds: {v}"));
                }
                cfg.interval = Duration::from_secs_f64(secs);
            }
            "--base" => cfg.base = Some(value("--base")?),
            "--no-watch" => cfg.watch = false,
            "--no-poll" => cfg.poll = false,
            "--no-herdr" => {
                herdr = false;
                cfg.herdr = false;
            }
            "--herdr-repos" => herdr_repos = true,
            "--host" => hosts.push(value("--host")?),
            "--nerd-font" => nerd_font = true,
            "--git-tui" => git_tui = Some(value("--git-tui")?).filter(|s| !s.trim().is_empty()),
            "--remote-cmd" => {
                remote_cmd = value("--remote-cmd")?;
                if remote_cmd.trim().is_empty() {
                    return Err("--remote-cmd needs a command".into());
                }
            }
            "--layout" => {
                layout = match value("--layout")?.as_str() {
                    "stacked" => LayoutMode::Stacked,
                    "side" => LayoutMode::Side,
                    other => return Err(format!("invalid layout: {other} (expected stacked or side)")),
                }
            }
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option: {f}")),
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    // HOST:PATH, unless it is a local directory (`./a:b` stays local).
    let mut remote = Vec::new();
    if !serve {
        paths.retain(|p| match remote_arg(p) {
            Some(r) => {
                remote.push(r);
                false
            }
            None => true,
        });
    }
    if serve && herdr_repos {
        return Err("serve doesn't take --herdr-repos (yet)".to_string());
    }
    if herdr_repos && !herdr {
        return Err("--herdr-repos needs herdr (drop --no-herdr)".to_string());
    }
    Ok(Some(Args {
        serve,
        paths,
        remote,
        hosts,
        cfg,
        layout,
        herdr,
        herdr_repos,
        git_tui,
        nerd_font,
        remote_cmd,
    }))
}

/// `HOST:PATH` as (host, path), like scp: a colon before any slash, and not a local directory.
/// An empty path is the home directory there.
fn remote_arg(arg: &Path) -> Option<(String, String)> {
    if arg.is_dir() {
        return None;
    }
    let s = arg.to_str()?;
    let (host, path) = s.split_once(':')?;
    if host.is_empty() || host.contains('/') {
        return None;
    }
    let path = if path.is_empty() { "." } else { path };
    Some((host.to_string(), path.to_string()))
}

/// `canopy remote install HOST [--dir DIR] [--binary FILE]`.
fn remote_install(args: &[String]) -> ExitCode {
    let mut host = None;
    let mut dir = canopy::remote::install::DEFAULT_DIR.to_string();
    let mut binary = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next().cloned());
        match flag {
            "--dir" => match value() {
                Some(v) => dir = v,
                None => return usage_error("--dir needs a value"),
            },
            "--binary" => match value() {
                Some(v) => binary = Some(PathBuf::from(v)),
                None => return usage_error("--binary needs a value"),
            },
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            f if f.starts_with('-') => return usage_error(&format!("unknown option: {f}")),
            h if host.is_none() => host = Some(h.to_string()),
            extra => return usage_error(&format!("unexpected argument: {extra}")),
        }
    }
    let Some(host) = host else { return usage_error("remote install needs a HOST") };
    let opts = canopy::remote::install::Install { host, dir, binary };
    match canopy::remote::install::run(&opts) {
        Ok(path) => {
            eprintln!("installed {path} on {}", opts.host);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("canopy remote install: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("canopy: {msg}\n\nRun 'canopy --help' for usage.");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().is_some_and(|a| a == "remote") {
        return match raw.get(1).map(String::as_str) {
            Some("install") => remote_install(&raw[2..]),
            _ => usage_error("remote: expected `remote install HOST`"),
        };
    }
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("canopy: {e}\n\nRun 'canopy --help' for usage.");
            return ExitCode::from(2);
        }
    };
    if args.serve {
        let paths = if args.paths.is_empty() { vec![PathBuf::from(".")] } else { args.paths };
        // Errors per path go to the client; this one means the protocol couldn't even start.
        return match canopy::remote::serve::run(paths, args.cfg, args.herdr) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("canopy serve: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    // One entry per repository: paths inside the same one (say, two of its worktrees) count once.
    let mut repos: Vec<canopy::git::RepoPaths> = Vec::new();
    if args.paths.is_empty() && args.remote.is_empty() && args.hosts.is_empty() {
        // The current directory, unless --herdr-repos finds the repositories (then it counts
        // only if it is in one).
        match canopy::git::discover(Path::new(".")) {
            Ok(p) => repos.push(p),
            Err(_) if args.herdr_repos => {}
            Err(e) => {
                eprintln!("canopy: {e:#}");
                return ExitCode::FAILURE;
            }
        }
    }
    for path in &args.paths {
        if !path.is_dir() {
            eprintln!("canopy: not a directory: {}", path.display());
            return ExitCode::FAILURE;
        }
        match canopy::git::discover(path) {
            Ok(p) if repos.iter().any(|r| r.common_dir == p.common_dir) => {}
            Ok(p) => repos.push(p),
            Err(e) => {
                eprintln!("canopy: {e:#}");
                return ExitCode::FAILURE;
            }
        }
    }
    // One connection per host, its paths in order.
    let mut remotes: Vec<(String, Vec<String>)> = Vec::new();
    for host in args.hosts {
        if !remotes.iter().any(|(h, _)| *h == host) {
            remotes.push((host, Vec::new()));
        }
    }
    for (host, path) in args.remote {
        match remotes.iter_mut().find(|(h, _)| *h == host) {
            Some((_, paths)) if paths.contains(&path) => {}
            Some((_, paths)) => paths.push(path),
            None => remotes.push((host, vec![path])),
        }
    }
    let opts = canopy::ui::Options {
        layout: args.layout,
        herdr: args.herdr,
        herdr_repos: args.herdr_repos,
        git_tui: args.git_tui,
        nerd_font: args.nerd_font,
        remote_cmd: args.remote_cmd,
    };
    match canopy::ui::run(repos, remotes, args.cfg, opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("canopy: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_arguments() {
        let r = |s: &str| remote_arg(Path::new(s));
        assert_eq!(r("gh-b-001:/home/me/proj"), Some(("gh-b-001".into(), "/home/me/proj".into())));
        assert_eq!(r("me@box:proj"), Some(("me@box".into(), "proj".into())));
        assert_eq!(r("box:"), Some(("box".into(), ".".into())));
        assert_eq!(r("./a:b"), None);
        assert_eq!(r("/abs/a:b"), None);
        assert_eq!(r(":x"), None);
        assert_eq!(r("plain"), None);
    }
}
