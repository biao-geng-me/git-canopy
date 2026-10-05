//! A shell in an inset: a pty running `$SHELL` in a worktree, emulated with `vt100` and drawn
//! into a panel below the graph. The fallback when neither herdr nor tmux can split a pane for it.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Lines kept above the screen for scrolling back with the mouse wheel.
const SCROLLBACK: usize = 2000;

/// Answers the queries shells and line editors send (cursor position, device attributes); the
/// replies are written back to the pty after each chunk of output.
#[derive(Default)]
pub struct Replies(Vec<u8>);

impl vt100::Callbacks for Replies {
    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, params: &[&[u16]], c: char) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, first) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.0.extend(format!("\x1b[{};{}R", row + 1, col + 1).bytes());
            }
            (None, 'n', 5) => self.0.extend(b"\x1b[0n"),
            (None, 'c', 0) => self.0.extend(b"\x1b[?62;22c"),
            (Some(b'>'), 'c', 0) => self.0.extend(b"\x1b[>0;0;0c"),
            _ => {}
        }
    }
}

pub type Parser = vt100::Parser<Replies>;

pub struct Term {
    /// Worktree the shell was started in.
    pub path: PathBuf,
    parser: Arc<Mutex<Parser>>,
    master: File,
    pid: i32,
    size: (u16, u16),
}

impl Term {
    /// Start `$SHELL` in `path` on a new pty of `rows` × `cols`, running `command` (with
    /// `$SHELL -c`) instead of an interactive shell if given. `on_output` is called (on the reader
    /// thread) after each chunk of output, `on_exit` once the shell is gone.
    pub fn spawn(
        path: &Path,
        command: Option<&str>,
        rows: u16,
        cols: u16,
        on_output: impl Fn() + Send + 'static,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Term> {
        let (rows, cols) = (rows.max(2), cols.max(10));
        let (master, slave) = openpty(rows, cols)?;
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = Command::new(&shell);
        if let Some(c) = command {
            cmd.arg("-c").arg(format!("exec {c}"));
        }
        cmd.current_dir(path)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // New session with the pty as its controlling terminal, so job control and
                // Ctrl-C work and the shell gets SIGHUP when the pty closes.
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let pid = child.id() as i32;

        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLLBACK,
            Replies::default(),
        )));
        let mut reader = master.try_clone()?;
        let mut replies = master.try_clone()?;
        let shared = parser.clone();
        std::thread::Builder::new().name("canopy-term".into()).spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            // EIO once the shell (and everything else holding the pty) has exited.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                let answer = {
                    let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
                    p.process(&buf[..n]);
                    std::mem::take(&mut p.callbacks_mut().0)
                };
                if !answer.is_empty() {
                    let _ = replies.write_all(&answer);
                }
                on_output();
            }
            let _ = child.wait();
            on_exit();
        })?;
        Ok(Term {
            path: path.to_path_buf(),
            parser,
            master,
            pid,
            size: (rows, cols),
        })
    }

    pub fn screen(&self) -> MutexGuard<'_, Parser> {
        self.parser.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Follow the panel size (the kernel sends the shell SIGWINCH).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(2), cols.max(10));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        self.screen().screen_mut().set_size(rows, cols);
        let ws = winsize(rows, cols);
        // SAFETY: TIOCSWINSZ with a valid winsize on our own pty master.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    }

    pub fn key(&mut self, k: KeyEvent) {
        let app_cursor = self.screen().screen().application_cursor();
        if let Some(bytes) = key_bytes(k, app_cursor) {
            // Typing jumps back from scrollback to the live screen.
            self.screen().screen_mut().set_scrollback(0);
            let _ = self.master.write_all(&bytes);
        }
    }

    /// Scroll back (`delta` > 0) or forward through the shell's history.
    pub fn scroll(&mut self, delta: isize) {
        let mut p = self.screen();
        let s = p.screen_mut();
        let to = (s.scrollback() as isize + delta).max(0) as usize;
        s.set_scrollback(to);
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // Hang up the shell's session; the reader thread reaps it.
        // SAFETY: plain kill(2) on the process group we created with setsid.
        unsafe { libc::kill(-self.pid, libc::SIGHUP) };
    }
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn openpty(rows: u16, cols: u16) -> std::io::Result<(File, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    let ws = winsize(rows, cols);
    // SAFETY: openpty fills in two fds we then own; name and termios may be null.
    let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both fds were just returned by openpty and are owned by nobody else.
    let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // Don't leak the master into the shell (or any other child).
    // SAFETY: fcntl on a valid fd.
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok((master, slave))
}

/// The bytes an xterm sends for a key.
pub fn key_bytes(k: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let m = k.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let param = 1 + m.contains(KeyModifiers::SHIFT) as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi = |code: &str, end: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[{};{param}{end}", if code.is_empty() { "1" } else { code }).into_bytes()
        } else {
            format!("\x1b[{code}{end}").into_bytes()
        }
    };
    let cursor = |end: char| -> Vec<u8> {
        if param == 1 && app_cursor {
            format!("\x1bO{end}").into_bytes()
        } else {
            csi("", end)
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[{n};{param}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let mut out = match k.code {
        KeyCode::Char(c) if ctrl => {
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                ' ' | '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '7' | '/' => 0x1f,
                '8' | '?' => 0x7f,
                _ => return None,
            };
            vec![b]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![if ctrl { 0x08 } else { 0x7f }],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Right => cursor('C'),
        KeyCode::Left => cursor('D'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let end = (b'P' + n - 1) as char;
            if param > 1 {
                csi("", end)
            } else {
                format!("\x1bO{end}").into_bytes()
            }
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        _ => return None,
    };
    // Alt sends ESC first for plain keys (modified cursor keys carry it in the parameter).
    let plain = matches!(
        k.code,
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Esc
    );
    if alt && plain {
        out.insert(0, 0x1b);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> Vec<u8> {
        key_bytes(KeyEvent::new(code, m), false).unwrap()
    }

    #[test]
    fn encodes_keys_like_xterm() {
        let none = KeyModifiers::NONE;
        assert_eq!(key(KeyCode::Char('é'), none), "é".as_bytes());
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(key(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(key(KeyCode::Enter, none), b"\r");
        assert_eq!(key(KeyCode::Backspace, none), [0x7f]);
        assert_eq!(key(KeyCode::Up, none), b"\x1b[A");
        assert_eq!(key_bytes(KeyEvent::new(KeyCode::Up, none), true).unwrap(), b"\x1bOA");
        assert_eq!(key(KeyCode::Right, KeyModifiers::CONTROL), b"\x1b[1;5C");
        assert_eq!(key(KeyCode::Delete, none), b"\x1b[3~");
        assert_eq!(key(KeyCode::PageUp, KeyModifiers::SHIFT), b"\x1b[5;2~");
        assert_eq!(key(KeyCode::F(1), none), b"\x1bOP");
        assert_eq!(key(KeyCode::F(5), none), b"\x1b[15~");
    }

    #[test]
    fn runs_a_shell_in_the_directory() {
        let dir = std::env::temp_dir().join(format!("canopy-term-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        let mut t = Term::spawn(
            &dir,
            None,
            10,
            60,
            move || {
                let _ = tx.send(false);
            },
            move || {
                let _ = tx2.send(true);
            },
        )
        .unwrap();
        for c in "pwd; exit\r".chars() {
            t.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !rx.recv_timeout(deadline - std::time::Instant::now()).unwrap() {}
        let text = t.screen().screen().contents();
        let canon = std::fs::canonicalize(&dir).unwrap();
        assert!(text.contains(canon.to_str().unwrap()), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runs_a_command_instead_of_a_shell() {
        let dir = std::env::temp_dir().join(format!("canopy-term-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        let t = Term::spawn(
            &dir,
            Some("printf 'ran in %s' \"$PWD\""),
            10,
            60,
            move || {
                let _ = tx.send(false);
            },
            move || {
                let _ = tx2.send(true);
            },
        )
        .unwrap();
        // It exits by itself once the command is done.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !rx.recv_timeout(deadline - std::time::Instant::now()).unwrap() {}
        let text = t.screen().screen().contents();
        assert!(text.contains("ran in "), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
