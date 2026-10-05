use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use canopy::ui::LayoutMode;
use canopy::watch::Config;

const USAGE: &str = "\
canopy - live view of every git worktree of a repo

USAGE:
    canopy [OPTIONS] [PATH]
    git canopy [OPTIONS] [PATH]

ARGS:
    PATH    any directory inside the repository (default: current directory)

OPTIONS:
    -i, --interval <SECS>   poll interval in seconds for worktrees that can't be watched (default:
                            60); watched worktrees are re-checked every 10 minutes regardless
        --no-poll           never poll; update only on file events and manual refresh (r)
        --base <BRANCH>     branch to compare worktrees against (default: main/master/...)
        --layout <MODE>     stacked (graph above worktrees, default) or side (side by side)
        --no-watch          disable filesystem notifications, poll only
        --no-herdr          don't show herdr agent activity
        --git-tui <CMD>     git TUI that t opens in a worktree (default: $CANOPY_GIT_TUI, else
                            the first of lazygit, gitui, tig on PATH)
    -h, --help              print help
    -V, --version           print version

KEYS:
    ↑↓/jk move · Space expand · Enter full diff · b diff vs base · t git TUI · s shell · y copy · ? help
";

struct Args {
    path: PathBuf,
    cfg: Config,
    layout: LayoutMode,
    herdr: bool,
    git_tui: Option<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut path: Option<PathBuf> = None;
    let mut cfg = Config::default();
    let mut layout = LayoutMode::default();
    let mut herdr = true;
    let mut git_tui = std::env::var("CANOPY_GIT_TUI").ok().filter(|s| !s.trim().is_empty());
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        let s = arg.to_string_lossy().into_owned();
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
                println!("canopy {}", env!("CARGO_PKG_VERSION"));
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
            "--no-herdr" => herdr = false,
            "--git-tui" => git_tui = Some(value("--git-tui")?).filter(|s| !s.trim().is_empty()),
            "--layout" => {
                layout = match value("--layout")?.as_str() {
                    "stacked" => LayoutMode::Stacked,
                    "side" => LayoutMode::Side,
                    other => return Err(format!("invalid layout: {other} (expected stacked or side)")),
                }
            }
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option: {f}")),
            _ => {
                if path.is_some() {
                    return Err("only one PATH may be given".into());
                }
                path = Some(PathBuf::from(arg));
            }
        }
    }
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    Ok(Some(Args {
        path,
        cfg,
        layout,
        herdr,
        git_tui,
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
    if !args.path.is_dir() {
        eprintln!("canopy: not a directory: {}", args.path.display());
        return ExitCode::FAILURE;
    }
    let paths = match canopy::git::discover(&args.path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("canopy: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    match canopy::ui::run(paths, args.cfg, args.layout, args.herdr, args.git_tui) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("canopy: {e:#}");
            ExitCode::FAILURE
        }
    }
}
