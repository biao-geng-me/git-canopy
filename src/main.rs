use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use canopy::ui::LayoutMode;
use canopy::watch::Config;

const USAGE: &str = "\
canopy - live view of every git worktree of a repo

USAGE:
    canopy [OPTIONS] [PATH...]
    git canopy [OPTIONS] [PATH...]
    canopy serve [OPTIONS] [PATH...]

ARGS:
    PATH    any directory inside a repository (default: current directory); several watch
            several repositories, listed in that order

    serve   watch the repositories for a canopy on another machine, which runs this over
            ssh and talks to it on stdin/stdout (-i, --no-poll, --base, --no-watch and
            --no-herdr apply)

OPTIONS:
    -i, --interval <SECS>   poll interval in seconds for worktrees that can't be watched (default:
                            60); watched worktrees are re-checked every 10 minutes regardless
        --no-poll           never poll; update only on file events and manual refresh (r)
        --base <BRANCH>     branch to compare worktrees against (default: main/master/...)
        --layout <MODE>     stacked (graph above worktrees, default) or side (side by side)
        --no-watch          disable filesystem notifications, poll only
        --no-herdr          don't show herdr agent activity
        --herdr-repos       also watch every repository a herdr agent or workspace is in, added
                            and removed as they come and go (PATH is optional then)
        --git-tui <CMD>     git TUI that t opens in a worktree (default: $CANOPY_GIT_TUI, else
                            the first of lazygit, gitui, tig on PATH)
        --nerd-font         Nerd Font icons for untracked files and folders (default:
                            $CANOPY_NERD_FONT=1)
    -h, --help              print help
    -V, --version           print version

KEYS:
    ↑↓/jk move · Space expand · Enter full diff · b diff vs base · o overlaps · t git TUI · s shell · y copy · ? help
";

struct Args {
    /// `canopy serve`.
    serve: bool,
    paths: Vec<PathBuf>,
    cfg: Config,
    layout: LayoutMode,
    herdr: bool,
    herdr_repos: bool,
    git_tui: Option<String>,
    nerd_font: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut cfg = Config::default();
    let mut layout = LayoutMode::default();
    let mut herdr = true;
    let mut herdr_repos = false;
    let mut git_tui = std::env::var("CANOPY_GIT_TUI").ok().filter(|s| !s.trim().is_empty());
    let mut nerd_font = std::env::var("CANOPY_NERD_FONT").is_ok_and(|v| v == "1");
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
            "--nerd-font" => nerd_font = true,
            "--git-tui" => git_tui = Some(value("--git-tui")?).filter(|s| !s.trim().is_empty()),
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
    if serve && herdr_repos {
        return Err("serve doesn't take --herdr-repos (yet)".to_string());
    }
    if herdr_repos && !herdr {
        return Err("--herdr-repos needs herdr (drop --no-herdr)".to_string());
    }
    Ok(Some(Args {
        serve,
        paths,
        cfg,
        layout,
        herdr,
        herdr_repos,
        git_tui,
        nerd_font,
    }))
}

fn main() -> ExitCode {
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
    if args.paths.is_empty() {
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
    let opts = canopy::ui::Options {
        layout: args.layout,
        herdr: args.herdr,
        herdr_repos: args.herdr_repos,
        git_tui: args.git_tui,
        nerd_font: args.nerd_font,
    };
    match canopy::ui::run(repos, args.cfg, opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("canopy: {e:#}");
            ExitCode::FAILURE
        }
    }
}
