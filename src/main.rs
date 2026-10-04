use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use canopy::watch::Config;

const USAGE: &str = "\
canopy - live view of every git worktree of a repo

USAGE:
    canopy [OPTIONS] [PATH]
    git canopy [OPTIONS] [PATH]

ARGS:
    PATH    any directory inside the repository (default: current directory)

OPTIONS:
    -i, --interval <SECS>   status poll interval in seconds (default: 1.5)
        --base <BRANCH>     branch to compare worktrees against (default: main/master/...)
        --no-watch          disable filesystem notifications, poll only
    -h, --help              print help
    -V, --version           print version

KEYS:
    ↑↓/jk move · Tab switch panel · Enter diff · Esc back · r refresh · ? help · q quit
";

struct Args {
    path: PathBuf,
    cfg: Config,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut path: Option<PathBuf> = None;
    let mut cfg = Config::default();
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
    Ok(Some(Args { path, cfg }))
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
    match canopy::ui::run(paths, args.cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("canopy: {e:#}");
            ExitCode::FAILURE
        }
    }
}
