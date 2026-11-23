//! Minimal PTY session: openpty + fork + setsid + TIOCSCTTY + execve.

use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
pub use nix::pty::Winsize;

use crate::error::{Error, Result};

nix::ioctl_write_ptr_bad!(tiocsctty_ioctl, libc::TIOCSCTTY, libc::c_int);
nix::ioctl_write_ptr_bad!(tiocswinsz_ioctl, libc::TIOCSWINSZ, Winsize);
nix::ioctl_read_bad!(tiocgwinsz_ioctl, libc::TIOCGWINSZ, Winsize);
nix::ioctl_write_ptr_bad!(tiocsti_ioctl, libc::TIOCSTI, libc::c_char);

/// A running shell (or any child) attached to a PTY we own.
pub struct Session {
    pub pid: i32,
    pub master: OwnedFd,
    pub slave_path: String,
    pub winsize: Winsize,
    raw: Vec<u8>,
    bytes_read: u64,
    /// When false, terminal queries are observed but not answered.
    responder_enabled: bool,
    /// Every query seen, answered or not.
    pub queries_seen: u64,
    eof: bool,
    /// Terminal queries answered so far (fish/vim/less block until answered).
    pub answered: Vec<crate::responder::Answered>,
    reaped: bool,
    exit: Option<i32>,
    exit_signal: Option<String>,
}

pub struct SpawnSpec<'a> {
    pub argv: &'a [String],
    pub env: &'a [(String, String)],
    pub winsize: Winsize,
    /// Working directory; must exist.
    pub cwd: &'a std::path::Path,
}

impl Session {
    /// Create a PTY and start `spec.argv` on the slave side.
    ///
    /// Child setup order matters and is the part most implementations get
    /// wrong:
    ///   1. `setsid()`      — new session, so no controlling terminal yet;
    ///   2. `TIOCSCTTY`     — make the slave *the* controlling terminal;
    ///   3. `dup2` x3       — 0/1/2 all point at the slave;
    ///   4. `execve`.
    ///
    /// All four are async-signal-safe, so doing them in `pre_exec` is legal.
    pub fn spawn(spec: SpawnSpec<'_>) -> Result<Session> {
        Session::spawn_inner(spec, true)
    }

    /// Same as `spawn`, but terminal queries are recorded and **not** answered.
    /// This exists to measure what the absence of a responder does.
    pub fn spawn_disabled_responder(spec: SpawnSpec<'_>) -> Result<Session> {
        Session::spawn_inner(spec, false)
    }

    fn spawn_inner(spec: SpawnSpec<'_>, responder_enabled: bool) -> Result<Session> {
        let openpty = nix::pty::openpty(Some(&spec.winsize), None).map_err(|e| Error::Sys {
            call: "openpty",
            source: e,
        })?;
        let master = openpty.master;
        let slave = openpty.slave;
        let slave_path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(Error::PlainIo)?;

        let slave_raw = slave.as_raw_fd();
        let argv_c: Vec<CString> = spec
            .argv
            .iter()
            .map(|a| CString::new(a.as_str()).unwrap_or_default())
            .collect();
        let env_c: Vec<CString> = spec
            .env
            .iter()
            .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
            .collect();
        let cwd = CString::new(spec.cwd.to_string_lossy().as_bytes())
            .map_err(|_| Error::Usage("cwd contains a NUL byte".into()))?;

        // SAFETY: the child branch below performs only async-signal-safe calls
        // before exec and never returns to Rust code with owned state.
        let fork_res = unsafe { nix::unistd::fork() };
        let pid = match fork_res.map_err(|e| Error::Sys {
            call: "fork",
            source: e,
        })? {
            nix::unistd::ForkResult::Parent { child } => child.as_raw(),
            nix::unistd::ForkResult::Child => {
                // Child: from here on only async-signal-safe calls.
                // SAFETY: all calls below are async-signal-safe and use only
                // values already materialised before the fork.
                unsafe {
                    if libc::setsid() == -1 {
                        libc::_exit(126);
                    }
                    // Make the slave our controlling terminal. arg 0 means
                    // "do not steal it if another session already has it".
                    if tiocsctty_ioctl(slave_raw, &0i32).is_err() {
                        libc::_exit(126);
                    }
                    for fd in 0..3 {
                        if libc::dup2(slave_raw, fd) == -1 {
                            libc::_exit(126);
                        }
                    }
                    if slave_raw > 2 {
                        libc::close(slave_raw);
                    }
                    if libc::chdir(cwd.as_ptr()) == -1 {
                        libc::_exit(126);
                    }
                    let mut argv_p: Vec<*const libc::c_char> =
                        argv_c.iter().map(|c| c.as_ptr()).collect();
                    argv_p.push(std::ptr::null());
                    let mut env_p: Vec<*const libc::c_char> =
                        env_c.iter().map(|c| c.as_ptr()).collect();
                    env_p.push(std::ptr::null());
                    libc::execvpe(argv_p[0], argv_p.as_ptr(), env_p.as_ptr());
                    libc::_exit(127);
                }
            }
        };

        // Parent: the slave fd is no longer needed once the child has it.
        drop(slave);
        set_nonblocking(master.as_raw_fd())?;

        let mut session = Session {
            pid,
            master,
            slave_path,
            winsize: spec.winsize,
            raw: Vec::new(),
            bytes_read: 0,
            responder_enabled: true,
            queries_seen: 0,
            eof: false,
            answered: Vec::new(),
            reaped: false,
            exit: None,
            exit_signal: None,
        };
        session.responder_enabled = responder_enabled;
        Ok(session)
    }

    /// Write raw bytes to the master (as if typed at the keyboard).
    pub fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        let mut written = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while written < bytes.len() {
            match nix::unistd::write(&self.master, &bytes[written..]) {
                Ok(n) if n > 0 => written += n,
                Ok(_) => break,
                Err(nix::errno::Errno::EAGAIN) => {
                    if Instant::now() > deadline {
                        return Err(Error::Sys {
                            call: "write(master)",
                            source: nix::errno::Errno::EAGAIN,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => {
                    return Err(Error::Sys {
                        call: "write(master)",
                        source: e,
                    })
                }
            }
        }
        Ok(())
    }

    /// Write a line and a trailing CR. The CR is what the terminal driver
    /// turns into NL for the line discipline; sending bare LF works for most
    /// shells but not for all readline configurations.
    pub fn send_line(&mut self, line: &str) -> Result<()> {
        let mut buf = line.as_bytes().to_vec();
        buf.push(b'\r');
        self.write_raw(&buf)
    }

    /// Read whatever is available, appending to the internal buffer.
    /// Returns the number of new bytes.
    pub fn pump(&mut self, want_ms: u64) -> Result<usize> {
        let mut total = 0;
        let deadline = Instant::now() + Duration::from_millis(want_ms);
        let mut first = true;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = if first {
                PollTimeout::try_from(remaining.max(Duration::from_millis(1)))
                    .unwrap_or(PollTimeout::ZERO)
            } else if remaining.is_zero() {
                break;
            } else {
                PollTimeout::try_from(remaining).unwrap_or(PollTimeout::ZERO)
            };
            first = false;
            let mut fds = [PollFd::new(self.master.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, timeout) {
                Ok(0) => break, // timeout, nothing more to read
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    return Err(Error::Sys {
                        call: "poll",
                        source: e,
                    })
                }
            }
            let revents = fds[0].revents().unwrap_or(PollFlags::empty());
            if revents.is_empty() {
                break;
            }
            // POLLHUP means the slave side closed: everything has been read.
            let mut eof = revents.contains(PollFlags::POLLHUP);
            let mut buf = [0u8; 8192];
            loop {
                match nix::unistd::read(self.master.as_raw_fd(), &mut buf) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        self.raw.extend_from_slice(&buf[..n]);
                        self.bytes_read += n as u64;
                        total += n;
                        // A PTY is a pipe, not a terminal: programs that query
                        // the terminal (fish, vim, less) block until answered.
                        let (reply, seen) = crate::responder::respond(
                            &buf[..n],
                            self.winsize.ws_row,
                            self.winsize.ws_col,
                        );
                        if !seen.is_empty() {
                            self.queries_seen = self.queries_seen.saturating_add(seen.len() as u64);
                        }
                        let reply = if self.responder_enabled {
                            reply
                        } else {
                            Vec::new()
                        };
                        if !seen.is_empty() && self.responder_enabled {
                            self.answered.extend(seen);
                        }
                        if !reply.is_empty() {
                            // Best effort: if the buffer is full the program
                            // will time out on its own, which is recorded.
                            let _ = nix::unistd::write(&self.master, &reply);
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) => break,
                    Err(nix::errno::Errno::EIO) => {
                        // EIO is the normal "slave side is gone" error on Linux.
                        eof = true;
                        break;
                    }
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(e) => {
                        return Err(Error::Sys {
                            call: "read(master)",
                            source: e,
                        })
                    }
                }
            }
            if eof {
                self.eof = true;
                break;
            }
            if total > 0 && Instant::now() >= deadline {
                break;
            }
        }
        Ok(total)
    }

    /// Pump until the stream has been quiet for `quiet_ms`, or `max_ms` passed.
    ///
    /// This is the only reliable "the shell has finished drawing its prompt"
    /// signal: prompt text is shell-specific and, for fish, is redrawn
    /// character by character with cursor-positioning sequences, so matching a
    /// literal prompt string is not portable.
    pub fn wait_quiet(&mut self, quiet_ms: u64, max_ms: u64) -> Result<u64> {
        let start = Instant::now();
        let mut last_len = self.raw.len();
        let mut quiet_since = Instant::now();
        loop {
            self.pump(quiet_ms.min(50))?;
            if self.raw.len() != last_len {
                last_len = self.raw.len();
                quiet_since = Instant::now();
            }
            if quiet_since.elapsed() >= Duration::from_millis(quiet_ms) {
                break;
            }
            if start.elapsed() >= Duration::from_millis(max_ms) {
                break;
            }
            if self.eof {
                break;
            }
        }
        Ok(start.elapsed().as_millis() as u64)
    }

    /// Read until `needle` appears anywhere in the accumulated stream
    /// (raw bytes, or after applying terminal backspaces).
    ///
    /// Both forms are tried because a redrawing line editor can split a token
    /// across cursor-movement sequences. Callers that need to assert "the
    /// command really ran" must use `util::count_bare_lines` on the text, not
    /// this early-exit check.
    pub fn read_until_any(&mut self, needle: &str, timeout: Duration) -> Result<ReadOutcome> {
        let start = Instant::now();
        loop {
            let raw = self.text();
            if raw.contains(needle) || debackspace(&raw).contains(needle) {
                return Ok(ReadOutcome {
                    matched: true,
                    elapsed: start.elapsed(),
                });
            }
            if start.elapsed() >= timeout {
                return Ok(ReadOutcome {
                    matched: false,
                    elapsed: start.elapsed(),
                });
            }
            self.pump(50)?;
        }
    }

    /// Read until `needle` appears in the accumulated output, or time out.
    /// Returns the whole buffer on success and on timeout (never panics on a
    /// missing needle; the caller decides).
    pub fn read_until(&mut self, needle: &str, timeout: Duration) -> Result<ReadOutcome> {
        let start = Instant::now();
        loop {
            if self.text().contains(needle) {
                return Ok(ReadOutcome {
                    matched: true,
                    elapsed: start.elapsed(),
                });
            }
            if start.elapsed() >= timeout {
                return Ok(ReadOutcome {
                    matched: false,
                    elapsed: start.elapsed(),
                });
            }
            self.pump(50)?;
        }
    }

    /// Accumulated output as lossy UTF-8 (for matching and reporting).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }

    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Clear the accumulated buffer; returns what was discarded.
    pub fn take_text(&mut self) -> String {
        let s = String::from_utf8_lossy(&self.raw).into_owned();
        self.raw.clear();
        s
    }

    /// Resize the window. The kernel delivers SIGWINCH to the foreground
    /// process group of the slave side, which is why the shell (or the program
    /// it runs) re-reads the geometry without an explicit signal from us.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: master fd is valid for the process lifetime of `self`.
        unsafe { tiocswinsz_ioctl(self.master.as_raw_fd(), &ws) }.map_err(|e| Error::Sys {
            call: "TIOCSWINSZ",
            source: e,
        })?;
        self.winsize = ws;
        Ok(())
    }

    pub fn query_winsize(&self) -> Result<Winsize> {
        let mut ws = Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: master fd valid; `ws` is a properly sized output buffer.
        unsafe { tiocgwinsz_ioctl(self.master.as_raw_fd(), &mut ws) }.map_err(|e| Error::Sys {
            call: "TIOCGWINSZ",
            source: e,
        })?;
        Ok(ws)
    }

    /// Check whether a real terminal takeover is even possible.
    ///
    /// Two mechanisms exist and both are expected to be unavailable here:
    ///  - `TIOCSTI`: inject bytes into *another* terminal's input queue.
    ///    Disabled by `dev.tty.legacy_tiocsti=0` on this kernel.
    ///  - `TIOCSCTTY` on someone else's terminal: refused because the fd is not
    ///    open in our session, and `arg=1` is additionally guarded.
    pub fn probe_takeover(&self, foreign_slave_path: &str) -> serde_json::Value {
        let mut out = serde_json::json!({
            "foreign_terminal": foreign_slave_path,
            "tiocsti_disabled_by_sysctl": std::fs::read_to_string(
                "/proc/sys/dev/tty/legacy_tiocsti").ok().map(|s| s.trim().to_string()),
        });
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(foreign_slave_path)
        {
            Ok(f) => {
                let fd = f.as_raw_fd();
                let byte = b'x' as libc::c_char;
                // SAFETY: fd is valid and open for writing.
                let r = unsafe { tiocsti_ioctl(fd, &byte) };
                out["open_foreign_slave"] = serde_json::json!("ok");
                out["TIOCSTI"] = serde_json::json!(match r {
                    Ok(_) => "ALLOWED (takeover possible!)".to_string(),
                    Err(e) => format!("denied: {e}"),
                });
                // SAFETY: fd valid; arg 1 = forcibly steal the terminal.
                let r2 = unsafe { tiocsctty_ioctl(fd, &1i32) };
                out["TIOCSCTTY_force"] = serde_json::json!(match r2 {
                    Ok(_) => "ALLOWED (takeover possible!)".to_string(),
                    Err(e) => format!("denied: {e}"),
                });
            }
            Err(e) => {
                out["open_foreign_slave"] = serde_json::json!(format!("denied: {e}"));
            }
        }
        out
    }

    /// Terminate the session: SIGKILL if `force`, else SIGTERM, then reap.
    pub fn shutdown(&mut self, force: bool) -> Result<Option<i32>> {
        if self.reaped {
            return Ok(self.exit);
        }
        let sig = if force {
            nix::sys::signal::Signal::SIGKILL
        } else {
            nix::sys::signal::Signal::SIGTERM
        };
        // Signal the whole process group: the shell's job control puts
        // children in their own groups, so signal both the leader and the
        // group it owns.
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.pid), sig);
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(self.pid), sig);
        self.reap(Duration::from_secs(3))
    }

    /// Wait for the child to exit, up to `timeout`. Returns the exit code, or
    /// `None` if it is still running (or died by signal).
    pub fn reap(&mut self, timeout: Duration) -> Result<Option<i32>> {
        if self.reaped {
            return Ok(self.exit);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(self.pid),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ) {
                Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                    self.reaped = true;
                    self.exit = Some(code);
                    self.exit_signal = None;
                    return Ok(Some(code));
                }
                Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, core)) => {
                    self.reaped = true;
                    self.exit = None;
                    self.exit_signal = Some(format!("{sig:?} core={core}"));
                    return Ok(None);
                }
                Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(_) => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(nix::errno::Errno::ECHILD) => {
                    self.reaped = true;
                    return Ok(self.exit);
                }
                Err(e) => {
                    return Err(Error::Sys {
                        call: "waitpid",
                        source: e,
                    })
                }
            }
        }
    }

    /// Whether the master has seen EOF (slave side closed / child gone).
    pub fn at_eof(&self) -> bool {
        self.eof
    }

    /// Byte offset into the raw stream, used to slice "what arrived during
    /// this step" out of the running transcript.
    pub fn mark(&self) -> usize {
        self.raw.len()
    }

    /// Raw bytes accumulated since `mark`, lossily decoded.
    pub fn since(&self, mark: usize) -> String {
        let slice = self.raw.get(mark.min(self.raw.len())..).unwrap_or(&[]);
        String::from_utf8_lossy(slice).into_owned()
    }

    /// Same as `since`, with control characters escaped for the report.
    pub fn since_escaped(&self, mark: usize) -> String {
        crate::util::escape_control(&self.since(mark))
    }

    /// Terminating signal recorded by the last successful `reap`, if any.
    pub fn exit_signal_string(&self) -> Option<String> {
        self.exit_signal.clone()
    }

    pub fn slave_path(&self) -> &str {
        &self.slave_path
    }

    pub fn queries_seen(&self) -> u64 {
        self.queries_seen
    }

    /// Every query that was answered, as JSON, for the report.
    pub fn answers_json(&self) -> Vec<serde_json::Value> {
        self.answered
            .iter()
            .map(|a| serde_json::json!({ "query": a.query, "reply": a.reply }))
            .collect()
    }
}

pub struct ReadOutcome {
    pub matched: bool,
    /// Time spent waiting.
    pub elapsed: Duration,
}

impl ReadOutcome {
    /// Milliseconds waited, for the step detail.
    pub fn elapsed_ms(&self) -> u64 {
        self.elapsed.as_millis() as u64
    }
}

/// Apply terminal `\b` (backspace) semantics to a stream: `a\bb` -> `b`.
///
/// This is needed because a redrawing line editor can emit the prompt text in
/// pieces with cursor movement between them, so the literal bytes contain the
/// substring but interleaved with control sequences.
pub fn debackspace(s: &str) -> String {
    let mut out: Vec<char> = Vec::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{8}' => {
                out.pop();
            }
            '\r' => {}
            c => out.push(c),
        }
    }
    out.into_iter().collect()
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    // SAFETY: `fd` is a valid open descriptor owned by the caller.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 {
            return Err(Error::Sys {
                call: "fcntl(F_GETFL)",
                source: nix::errno::Errno::last(),
            });
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(Error::Sys {
                call: "fcntl(F_SETFL)",
                source: nix::errno::Errno::last(),
            });
        }
    }
    Ok(())
}
