//! Supervised interactive login for providers whose CLI continues into its chat
//! after sign-in (Gemini). The CLI runs on a private pty of this terminal's
//! size minus one row; the reserved bottom row carries a one-line, clickable
//! (OSC 8) copy of the sign-in link and an OSC 52 clipboard copy. As soon as
//! `ready()` reports the credentials, the CLI is stopped and the login ends
//! successfully. The link is only ever written to the terminal, never to disk.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::{Duration, Instant};

const SCAN_LIMIT: usize = 64 * 1024;
const GRACE: Duration = Duration::from_millis(1500);

/// Text with terminal control sequences removed (CSI, OSC, other escapes).
pub fn strip_ansi(input: &[u8]) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == 0x1b {
            i += 1;
            match input.get(i) {
                Some(b'[') => {
                    i += 1;
                    while i < input.len() && !(0x40..=0x7e).contains(&input[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    i += 1;
                    while i < input.len() && input[i] != 0x07 {
                        if input[i] == 0x1b && input.get(i + 1) == Some(&b'\\') {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    i += 1;
                }
                Some(_) => i += 1,
                None => {}
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The sign-in URL in visible text, rejoined when the CLI hard-wrapped it at
/// `cols` columns (a line that fills the width continues on the next line).
pub fn find_auth_url(text: &str, cols: usize) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let (start, offset) = lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, l)| l.find("https://").map(|o| (i, o)))?;
    let url_char = |c: char| c.is_ascii_graphic();
    let mut url = String::new();
    let first = &lines[start][offset..];
    let end = first.find(|c: char| !url_char(c)).unwrap_or(first.len());
    url.push_str(&first[..end]);
    let mut filled = end == first.len() && lines[start].chars().count() + 4 >= cols;
    let mut next = start + 1;
    while filled && next < lines.len() {
        let line = lines[next];
        let end = line.find(|c: char| !url_char(c)).unwrap_or(line.len());
        if end == 0 {
            break;
        }
        url.push_str(&line[..end]);
        filled = end == line.len() && line.chars().count() + 4 >= cols;
        next += 1;
    }
    (url.len() > "https://".len()).then_some(url)
}

/// Reserved-row content: waiting hint, or a short clickable label whose target
/// is the full URL, plus an OSC 52 copy when `copy` is set.
pub fn status_row(url: Option<&str>, row: u16, copy: bool) -> String {
    let mut s = format!("\x1b7\x1b[{row};1H\x1b[2K");
    match url {
        None => s.push_str("\x1b[2mWaiting for the sign-in link...\x1b[0m"),
        Some(url) => {
            let safe: String = url.chars().filter(|c| c.is_ascii_graphic()).collect();
            s.push_str(&format!(
                "\x1b]8;;{safe}\x07\x1b[4mOpen the sign-in link\x1b[0m\x1b]8;;\x07 (copied to clipboard if supported)"
            ));
            if copy {
                s.push_str(&format!("\x1b]52;c;{}\x07", base64(safe.as_bytes())));
            }
        }
    }
    s.push_str("\x1b8");
    s
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn winsize(fd: i32) -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 2 && ws.ws_col > 0
    {
        (ws.ws_row, ws.ws_col)
    } else {
        (24, 80)
    }
}

fn set_winsize(fd: i32, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
}

struct Raw(libc::termios);
impl Raw {
    fn enter() -> Result<Self> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        anyhow::ensure!(unsafe { libc::tcgetattr(0, &mut t) } == 0, "not a terminal");
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        Ok(Self(saved))
    }
}
impl Drop for Raw {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

fn write_all_fd(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            return;
        }
        data = &data[n as usize..];
    }
}

/// Run `command` on a pty until it exits or `ready()` turns true. Returns Ok
/// when `ready()` is true at the end; the CLI is stopped once it first is.
pub fn supervise(command: std::process::Command, ready: impl Fn() -> bool) -> Result<()> {
    let (ok, _) = supervise_capture(command, ready)?;
    if ok {
        Ok(())
    } else {
        bail!("Login did not complete: the CLI exited before sign-in finished")
    }
}

/// As `supervise`, also returning whether the login completed and the last terminal
/// output (bounded, control sequences stripped) so a caller can recognize a failure.
pub fn supervise_capture(
    mut command: std::process::Command,
    ready: impl Fn() -> bool,
) -> Result<(bool, String)> {
    let (mut rows, mut cols) = winsize(0);
    let (mut master, mut slave) = (0, 0);
    let ws = libc::winsize {
        ws_row: rows - 1,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    anyhow::ensure!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &ws,
            )
        } == 0,
        "could not open a pty"
    );
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("could not start the login command")?;
    let pid = child.id() as i32;
    let raw = Raw::enter()?;
    let mut scan: Vec<u8> = Vec::new();
    let mut url: Option<String> = None;
    let mut ready_at: Option<Instant> = None;
    let mut last_row = Instant::now() - Duration::from_secs(10);
    let mfd = master.as_raw_fd();
    let mut buf = [0u8; 8192];
    let mut done = false;
    let mut exited = false;
    write_all_fd(1, status_row(None, rows, false).as_bytes());
    while !done {
        let (r, c) = winsize(0);
        if (r, c) != (rows, cols) {
            (rows, cols) = (r, c);
            set_winsize(mfd, rows - 1, cols);
            unsafe { libc::kill(-pid, libc::SIGWINCH) };
        }
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: mfd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe { libc::poll(fds.as_mut_ptr(), 2, 250) };
        if fds[0].revents & libc::POLLIN != 0 {
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                write_all_fd(mfd, &buf[..n as usize]);
            }
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = unsafe { libc::read(mfd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let chunk = &buf[..n as usize];
                write_all_fd(1, chunk);
                scan.extend_from_slice(chunk);
                if scan.len() > SCAN_LIMIT {
                    scan.drain(..scan.len() - SCAN_LIMIT);
                }
                let found = find_auth_url(&strip_ansi(&scan), cols as usize);
                if found.is_some() && found != url {
                    write_all_fd(1, status_row(found.as_deref(), rows, true).as_bytes());
                    url = found;
                    last_row = Instant::now();
                }
            } else if n <= 0 && fds[1].revents & libc::POLLHUP != 0 {
                exited = true;
            }
        }
        if last_row.elapsed() > Duration::from_secs(1) {
            // The CLI may clear the screen; keep the reserved row drawn.
            write_all_fd(1, status_row(url.as_deref(), rows, false).as_bytes());
            last_row = Instant::now();
        }
        if child.try_wait()?.is_some() {
            exited = true;
        }
        if ready() {
            let since = *ready_at.get_or_insert_with(Instant::now);
            if since.elapsed() >= GRACE {
                done = true;
            }
        }
        if exited {
            done = true;
        }
    }
    if child.try_wait()?.is_none() {
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while child.try_wait()?.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if child.try_wait()?.is_none() {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            let _ = child.wait();
        }
    }
    // Leave the screen in a sane state: main screen, modes off, cursor shown.
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[<u\x1b[r\x1b[?25h\x1b[0m\x1b[2J\x1b[H");
    let _ = out.flush();
    drop(raw);
    let ok = ready();
    if ok {
        println!("Login complete.");
    }
    Ok((ok, strip_ansi(&scan)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_control_sequences() {
        let t = strip_ansi(b"\x1b[2J\x1b[1;1Hhi \x1b]8;;x\x07there\x1b[0m");
        assert_eq!(t, "hi there");
    }

    #[test]
    fn rejoins_a_hard_wrapped_url_and_stops_at_prose() {
        let text = "Visit:\r\nhttps://accounts.google.com/o/oauth2/v2/auth?client_id=abc&\r\nredirect_uri=https%3A%2F%2Fcodeassist.google.com&state=xyz\r\n\r\nEnter the code:";
        let url = find_auth_url(text, 62).unwrap();
        assert_eq!(
            url,
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=abc&redirect_uri=https%3A%2F%2Fcodeassist.google.com&state=xyz"
        );
    }

    #[test]
    fn does_not_join_a_short_line_with_the_next() {
        let url = find_auth_url("https://example.invalid/a\nnextword", 80).unwrap();
        assert_eq!(url, "https://example.invalid/a");
        assert!(find_auth_url("no link here", 80).is_none());
    }

    #[test]
    fn status_row_links_and_copies_without_the_url_as_visible_text() {
        let row = status_row(Some("https://example.invalid/x"), 24, true);
        assert!(row.contains("\x1b]8;;https://example.invalid/x\x07"));
        assert!(row.contains("\x1b]52;c;aHR0cHM6Ly9leGFtcGxlLmludmFsaWQveA==\x07"));
        assert!(row.contains("Open the sign-in link"));
        assert!(!status_row(Some("https://example.invalid/x"), 24, false).contains("]52"));
        assert!(status_row(None, 24, true).contains("Waiting"));
    }

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }

    #[test]
    fn supervise_stops_a_process_that_keeps_running_once_ready() {
        // Needs a controlling terminal for stdin; skipped when none exists.
        if unsafe { libc::isatty(0) } != 1 {
            return;
        }
        let mut c = std::process::Command::new("sleep");
        c.arg("30");
        let start = Instant::now();
        supervise(c, || true).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
