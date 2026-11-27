//! Non-privileged PTY session supervisor (P3-01 .. P3-05).
//!
//! Opens a fresh pseudoterminal pair (`rustix::pty`) and runs `/usr/bin/setsid --ctty`
//! so the child receives its own session and controlling terminal with **zero `unsafe`**.
//! Never attaches to or takes over pre-existing foreign terminals (`owns_this_terminal`
//! is `true` only for sessions created by this supervisor).

pub mod responder;

use std::io::ErrorKind;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

use self::responder::{AnsweredQuery, respond};

#[derive(Debug, Clone)]
pub struct PtyCapabilities {
    pub owns_this_terminal: bool,
    pub can_resize: bool,
    pub responder_enabled: bool,
    pub foreign_terminal_takeover_supported: bool,
}

impl PtyCapabilities {
    pub fn managed() -> Self {
        Self {
            owns_this_terminal: true,
            can_resize: true,
            responder_enabled: true,
            foreign_terminal_takeover_supported: false,
        }
    }

    /// Foreign terminals may be readable as files by the same uid, but terminal
    /// takeover (`TIOCSTI` / `TIOCSCTTY`) is intentionally unsupported (0-D §5.5).
    pub fn foreign_terminal() -> Self {
        Self {
            owns_this_terminal: false,
            can_resize: false,
            responder_enabled: false,
            foreign_terminal_takeover_supported: false,
        }
    }
}

pub struct ManagedPty {
    master: OwnedFd,
    child: Child,
    rows: u16,
    cols: u16,
    pub answered_queries: Vec<AnsweredQuery>,
    pub output_bytes: Vec<u8>,
}

impl ManagedPty {
    /// Spawns `program` with `args` under a freshly allocated PTY and `setsid --ctty`.
    /// Fails loudly if `zsh_module_dir` is provided and missing (P3-05).
    pub fn spawn(
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
        rows: u16,
        cols: u16,
        zsh_module_dir: Option<&Path>,
    ) -> Result<Self, String> {
        if let Some(mod_dir) = zsh_module_dir
            && !mod_dir.exists()
        {
            return Err(format!(
                "zsh module path missing: {} (refusing silent ZLE stall)",
                mod_dir.display()
            ));
        }

        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
            .map_err(|e| format!("openpt: {e}"))?;
        grantpt(&master).map_err(|e| format!("grantpt: {e}"))?;
        unlockpt(&master).map_err(|e| format!("unlockpt: {e}"))?;

        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        tcsetwinsize(&master, winsize).map_err(|e| format!("tcsetwinsize: {e}"))?;

        let slave = ioctl_tiocgptpeer(&master, OpenptFlags::RDWR | OpenptFlags::NOCTTY)
            .map_err(|e| format!("tiocgptpeer: {e}"))?;

        // Set master non-blocking for responder pumping.
        let fl = fcntl_getfl(&master).map_err(|e| format!("fcntl_getfl: {e}"))?;
        fcntl_setfl(&master, fl | OFlags::NONBLOCK).map_err(|e| format!("fcntl_setfl: {e}"))?;

        let slave_in = slave.try_clone().map_err(|e| e.to_string())?;
        let slave_out = slave.try_clone().map_err(|e| e.to_string())?;
        let slave_err = slave;

        let mut cmd = Command::new("/usr/bin/setsid");
        cmd.arg("--ctty")
            .arg(program)
            .args(args)
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err))
            .env("TERM", "xterm-256color");
        for (k, v) in envs {
            cmd.env(k, v);
        }

        let child = cmd
            .spawn()
            .map_err(|e| format!("spawn `{program}` on PTY: {e}"))?;

        Ok(Self {
            master,
            child,
            rows,
            cols,
            answered_queries: Vec::new(),
            output_bytes: Vec::new(),
        })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), String> {
        self.rows = rows;
        self.cols = cols;
        tcsetwinsize(
            &self.master,
            Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .map_err(|e| format!("resize tcsetwinsize: {e}"))
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut off = 0usize;
        while off < bytes.len() {
            match rustix::io::write(&self.master, &bytes[off..]) {
                Ok(0) => break,
                Ok(n) => off += n,
                Err(rustix::io::Errno::AGAIN) => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => return Err(format!("pty write: {e}")),
            }
        }
        Ok(())
    }

    /// Pumps PTY output for up to `duration`, answering any terminal queries.
    pub fn pump(&mut self, duration: Duration, quiet: Option<Duration>) -> Result<(), String> {
        let deadline = Instant::now() + duration;
        let mut last_io = Instant::now();
        let mut buf = [0u8; 8192];

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if let Some(q) = quiet
                && now.duration_since(last_io) >= q
            {
                break;
            }

            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: 20_000_000,
            };
            let (has_in, has_hup) = {
                let mut fds = [PollFd::new(&self.master, PollFlags::IN)];
                let ready = poll(&mut fds, Some(&timeout)).unwrap_or(0);
                let rev = fds[0].revents();
                (
                    ready > 0 && rev.contains(PollFlags::IN),
                    rev.contains(PollFlags::HUP),
                )
            };
            if has_in {
                loop {
                    match rustix::io::read(&self.master, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            last_io = Instant::now();
                            let chunk = &buf[..n];
                            self.output_bytes.extend_from_slice(chunk);
                            let (reply, seen) = respond(chunk, self.rows, self.cols);
                            if !seen.is_empty() {
                                self.answered_queries.extend(seen);
                            }
                            if !reply.is_empty() {
                                let _ = self.write_input(&reply);
                            }
                        }
                        Err(rustix::io::Errno::AGAIN) => break,
                        Err(e) => {
                            let io_err = std::io::Error::from(e);
                            if io_err.kind() == ErrorKind::Interrupted {
                                continue;
                            }
                            // EIO on master means slave closed.
                            return Ok(());
                        }
                    }
                }
            }
            if has_hup {
                break;
            }
        }
        Ok(())
    }

    pub fn wait(mut self) -> Result<(i32, Vec<u8>, Vec<AnsweredQuery>), String> {
        let _ = self.pump(Duration::from_millis(500), Some(Duration::from_millis(80)));
        let status = self.child.wait().map_err(|e| format!("wait child: {e}"))?;
        Ok((
            status.code().unwrap_or(1),
            self.output_bytes,
            self.answered_queries,
        ))
    }
}
