//! Copying text to the system clipboard, without dependencies: a local clipboard program when
//! there is one (`clip.exe` on WSL, `pbcopy`, `wl-copy`, `xclip`/`xsel`), else tmux's buffer
//! (which tmux forwards to the outer terminal's clipboard), else OSC 52 written to the terminal,
//! which most terminals turn into a clipboard write and which also works over SSH.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// A local clipboard program reading the text on stdin.
    Program {
        cmd: &'static str,
        args: &'static [&'static str],
        /// `clip.exe` reads UTF-16LE (UTF-8 would come out as mojibake); no BOM, it would be kept
        /// as a character.
        utf16: bool,
    },
    /// `tmux load-buffer -w -` (tmux ≥ 3.2): a tmux buffer, sent on to the outer terminal's
    /// clipboard per tmux's `set-clipboard`.
    Tmux,
    /// OSC 52 written to the terminal ([`osc52`]); only the UI thread may write it.
    Terminal,
}

/// The methods to try, best first; always ends with [`Method::Terminal`].
pub fn methods() -> Vec<Method> {
    methods_with(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()), on_path)
}

fn methods_with(env: impl Fn(&str) -> bool, on_path: impl Fn(&str) -> bool) -> Vec<Method> {
    let mut out = Vec::new();
    // Over SSH a local program would fill the remote machine's clipboard: use the terminal.
    let remote = env("SSH_CONNECTION") || env("SSH_TTY");
    if !remote {
        let program = |cmd, args, utf16| Method::Program { cmd, args, utf16 };
        let wsl = env("WSL_DISTRO_NAME") || env("WSL_INTEROP");
        if wsl && on_path("clip.exe") {
            out.push(program("clip.exe", &[], true));
        } else if cfg!(target_os = "macos") && on_path("pbcopy") {
            out.push(program("pbcopy", &[], false));
        } else if env("WAYLAND_DISPLAY") && on_path("wl-copy") {
            out.push(program("wl-copy", &[], false));
        } else if env("DISPLAY") && on_path("xclip") {
            out.push(program("xclip", &["-selection", "clipboard"], false));
        } else if env("DISPLAY") && on_path("xsel") {
            out.push(program("xsel", &["--clipboard", "--input"], false));
        }
    }
    if env("TMUX") {
        out.push(Method::Tmux);
    }
    out.push(Method::Terminal);
    out
}

fn on_path(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| is_file(&d.join(cmd))))
}

fn is_file(p: &Path) -> bool {
    p.metadata().is_ok_and(|m| m.is_file())
}

/// Copy with a program or tmux (blocking). `false` if it failed or `m` is [`Method::Terminal`].
pub fn run(m: &Method, text: &str) -> bool {
    let (mut cmd, input) = match m {
        Method::Program { cmd, args, utf16 } => {
            let mut c = Command::new(cmd);
            c.args(*args);
            (
                c,
                if *utf16 {
                    utf16le(text)
                } else {
                    text.as_bytes().to_vec()
                },
            )
        }
        Method::Tmux => {
            let mut c = Command::new("tmux");
            c.args(["load-buffer", "-w", "-"]);
            (c, text.as_bytes().to_vec())
        }
        Method::Terminal => return false,
    };
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let wrote = child.stdin.take().is_some_and(|mut s| s.write_all(&input).is_ok());
    child.wait().is_ok_and(|s| s.success()) && wrote
}

/// The OSC 52 sequence that sets the clipboard to `text`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in cases {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn utf16_without_bom() {
        assert_eq!(utf16le("aé"), [b'a', 0, 0xE9, 0]);
    }

    #[test]
    fn picks_methods_by_environment() {
        let pick = |vars: &[&str], progs: &[&str]| {
            methods_with(|k| vars.contains(&k), |c| progs.contains(&c))
                .into_iter()
                .map(|m| match m {
                    Method::Program { cmd, .. } => cmd,
                    Method::Tmux => "tmux",
                    Method::Terminal => "osc52",
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pick(&["WSL_DISTRO_NAME", "TMUX"], &["clip.exe"]),
            ["clip.exe", "tmux", "osc52"]
        );
        assert_eq!(pick(&["WSL_DISTRO_NAME"], &[]), ["osc52"]);
        assert_eq!(pick(&["DISPLAY"], &["xsel"]), ["xsel", "osc52"]);
        // Over SSH, never a local program.
        assert_eq!(
            pick(&["SSH_CONNECTION", "DISPLAY", "TMUX"], &["xclip"]),
            ["tmux", "osc52"]
        );
    }
}
