//! Shared primitives for Phase 0 / experiment 0-E.
//!
//! Three observation surfaces live here: procfs parsing, the raw inotify
//! interface, and the `NETLINK_CONNECTOR` process-event connector. They are
//! written against `libc` (not a convenience wrapper) because this experiment
//! is about *what the kernel actually delivers*: we need the raw mask, the
//! raw cookie, the raw `wd`, and control over the read buffer size.
//!
//! `unsafe` appears only where a kernel ABI forces it (syscalls and socket
//! calls with no safe wrapper in `libc`); each block carries a comment.

use std::fmt;
use std::fs;
use std::io;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Error {
    Io {
        path: String,
        source: io::Error,
    },
    Parse {
        what: &'static str,
        detail: String,
    },
    Sys {
        what: &'static str,
        errno: i32,
    },
    Usage(String),
}

impl Error {
    pub fn io(path: impl Into<String>, source: io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn sys(what: &'static str) -> Self {
        Error::Sys {
            what,
            errno: io::Error::last_os_error().raw_os_error().unwrap_or(0),
        }
    }

    /// errno value if this is a kernel-call failure, else `None`.
    pub fn errno(&self) -> Option<i32> {
        match self {
            Error::Sys { errno, .. } => Some(*errno),
            Error::Io { source, .. } => source.raw_os_error(),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{path}: {source}"),
            Error::Parse { what, detail } => write!(f, "cannot parse {what}: {detail}"),
            Error::Sys { what, errno } => {
                let text = io::Error::from_raw_os_error(*errno);
                write!(f, "{what} failed: errno {errno} ({text})")
            }
            Error::Usage(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// procfs
// ---------------------------------------------------------------------------

/// The parts of `/proc/<pid>/stat` this experiment relies on.
///
/// `comm` is kept because it is what a human reads, and `starttime` because it
/// is the only per-process value in `stat` that survives exec and is not reused
/// with the pid (see `REPORT.md` §5.2 for the measurement that establishes this).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: i32,
    pub comm: String,
    pub state: char,
    pub ppid: i32,
    /// Field 22: time since boot the process started, in clock ticks.
    pub starttime: u64,
}

impl ProcStat {
    /// The identity key this experiment argues for: pid alone is not unique
    /// across time, `(pid, starttime)` is.
    pub fn key(&self) -> ProcKey {
        ProcKey {
            pid: self.pid,
            starttime: self.starttime,
        }
    }

    pub fn is_zombie(&self) -> bool {
        self.state == 'Z' || self.state == 'X'
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcKey {
    pub pid: i32,
    pub starttime: u64,
}

impl fmt::Display for ProcKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.pid, self.starttime)
    }
}

/// Parse `/proc/<pid>/stat`.
///
/// `comm` is wrapped in parentheses and may itself contain spaces **and**
/// parentheses, so the field split must start from the *last* `)`. This is the
/// classic way to get this wrong, and getting it wrong silently shifts
/// `starttime`, which would corrupt the identity key.
pub fn parse_stat(content: &str) -> Result<ProcStat> {
    let open = content.find('(').ok_or_else(|| Error::Parse {
        what: "stat",
        detail: "no '('".into(),
    })?;
    let close = content.rfind(')').ok_or_else(|| Error::Parse {
        what: "stat",
        detail: "no ')'".into(),
    })?;
    if close < open {
        return Err(Error::Parse {
            what: "stat",
            detail: "')' before '('".into(),
        });
    }
    let pid: i32 = content[..open]
        .trim()
        .parse()
        .map_err(|e| Error::Parse {
            what: "stat pid",
            detail: format!("{e}"),
        })?;
    let comm = content[open + 1..close].to_string();
    let rest = &content[close + 1..];
    let f: Vec<&str> = rest.split_ascii_whitespace().collect();
    // `rest` begins at field 3 (state); therefore field N is f[N - 3].
    // starttime is field 22 -> index 19.
    const STARTTIME_IDX: usize = 19;
    if f.len() <= STARTTIME_IDX {
        return Err(Error::Parse {
            what: "stat",
            detail: format!("only {} fields after comm", f.len()),
        });
    }
    let state = f[0].chars().next().ok_or_else(|| Error::Parse {
        what: "stat state",
        detail: "empty".into(),
    })?;
    let ppid: i32 = f[1].parse().map_err(|e| Error::Parse {
        what: "stat ppid",
        detail: format!("{e}"),
    })?;
    let starttime: u64 = f[STARTTIME_IDX].parse().map_err(|e| Error::Parse {
        what: "stat starttime",
        detail: format!("{e}"),
    })?;
    Ok(ProcStat {
        pid,
        comm,
        state,
        ppid,
        starttime,
    })
}

pub fn read_stat(pid: i32) -> Result<ProcStat> {
    let path = format!("/proc/{pid}/stat");
    let content = fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
    parse_stat(&content)
}

/// Read one field out of `/proc/<pid>/status` (`Name: value` lines).
pub fn read_status_field(pid: i32, field: &str) -> Result<Option<String>> {
    let path = format!("/proc/{pid}/status");
    let content = fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            if let Some(value) = rest.strip_prefix(':') {
                return Ok(Some(value.trim().to_string()));
            }
        }
    }
    Ok(None)
}

/// Enumerate numeric pids currently visible in `/proc`.
pub fn list_pids() -> Result<Vec<i32>> {
    let dir = fs::read_dir("/proc").map_err(|e| Error::io("/proc", e))?;
    let mut pids = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|e| Error::io("/proc", e))?;
        if let Some(name) = entry.file_name().to_str() {
            if let Ok(pid) = name.parse::<i32>() {
                pids.push(pid);
            }
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

/// One full scan of `/proc`, the unit of cost in this experiment.
#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    pub procs: Vec<ProcStat>,
    /// Read failures, kept rather than swallowed: they are a result
    /// (other-uid processes, races, zombies during teardown).
    pub failures: Vec<(i32, &'static str, i32)>,
}

/// How much of each process to read. Cost scales with this, so the experiment
/// measures both variants instead of assuming one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanDepth {
    /// `/proc/<pid>/stat` only.
    Stat,
    /// `stat` + `status` + `cmdline` (what the task asked for).
    Full,
}

pub fn scan(depth: ScanDepth) -> Result<ScanResult> {
    let mut out = ScanResult::default();
    for pid in list_pids()? {
        match read_stat(pid) {
            Ok(st) => out.procs.push(st),
            Err(e) => {
                let errno = e.errno().unwrap_or(0);
                // ENOENT here is the process exiting mid-scan, which is itself
                // an observation: the sampler races the kernel.
                if errno != libc::ENOENT {
                    out.failures.push((pid, "stat", errno));
                }
                continue;
            }
        }
        if depth == ScanDepth::Full {
            if let Err(e) = read_status_field(pid, "Uid") {
                out.failures.push((pid, "status", e.errno().unwrap_or(0)));
            }
            let cmdline = format!("/proc/{pid}/cmdline");
            if let Err(e) = fs::read(&cmdline) {
                out.failures.push((pid, "cmdline", e.io_errno()));
            }
        }
    }
    Ok(out)
}

trait IoErrno {
    fn io_errno(&self) -> i32;
}

impl IoErrno for io::Error {
    fn io_errno(&self) -> i32 {
        self.raw_os_error().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Timing / statistics
// ---------------------------------------------------------------------------

/// Percentile of an unsorted sample, nearest-rank method.
///
/// Nearest-rank is chosen deliberately: it always returns a value that was
/// actually measured, never an interpolation between two measurements.
pub fn percentile(samples: &[u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(v.len() - 1);
    v[idx]
}

pub fn mean(samples: &[u64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<u64>() as f64 / samples.len() as f64
}

// ---------------------------------------------------------------------------
// inotify (raw)
// ---------------------------------------------------------------------------

pub mod ino {
    use super::{Error, Result};
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub const IN_ACCESS: u32 = 0x0000_0001;
    pub const IN_MODIFY: u32 = 0x0000_0002;
    pub const IN_ATTRIB: u32 = 0x0000_0004;
    pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
    pub const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
    pub const IN_OPEN: u32 = 0x0000_0020;
    pub const IN_MOVED_FROM: u32 = 0x0000_0040;
    pub const IN_MOVED_TO: u32 = 0x0000_0080;
    pub const IN_CREATE: u32 = 0x0000_0100;
    pub const IN_DELETE: u32 = 0x0000_0200;
    pub const IN_DELETE_SELF: u32 = 0x0000_0400;
    pub const IN_MOVE_SELF: u32 = 0x0000_0800;
    pub const IN_UNMOUNT: u32 = 0x0000_2000;
    pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
    pub const IN_IGNORED: u32 = 0x0000_8000;
    pub const IN_ONLYDIR: u32 = 0x0100_0000;
    pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
    pub const IN_EXCL_UNLINK: u32 = 0x0400_0000;
    pub const IN_ISDIR: u32 = 0x4000_0000;

    /// Mask used for directory watching in this experiment: everything except
    /// the high-volume `IN_ACCESS`/`IN_OPEN` noise unless explicitly asked for.
    pub fn default_mask() -> u32 {
        IN_MODIFY
            | IN_ATTRIB
            | IN_CLOSE_WRITE
            | IN_MOVED_FROM
            | IN_MOVED_TO
            | IN_CREATE
            | IN_DELETE
            | IN_DELETE_SELF
            | IN_MOVE_SELF
    }

    pub struct Inotify {
        fd: OwnedFd,
    }

    #[derive(Clone, Debug)]
    pub struct Event {
        pub wd: i32,
        pub mask: u32,
        pub cookie: u32,
        pub name: String,
    }

    impl Event {
        pub fn has(&self, bit: u32) -> bool {
            self.mask & bit != 0
        }

        pub fn mask_names(&self) -> String {
            const NAMES: &[(u32, &str)] = &[
                (IN_ACCESS, "ACCESS"),
                (IN_MODIFY, "MODIFY"),
                (IN_ATTRIB, "ATTRIB"),
                (IN_CLOSE_WRITE, "CLOSE_WRITE"),
                (IN_CLOSE_NOWRITE, "CLOSE_NOWRITE"),
                (IN_OPEN, "OPEN"),
                (IN_MOVED_FROM, "MOVED_FROM"),
                (IN_MOVED_TO, "MOVED_TO"),
                (IN_CREATE, "CREATE"),
                (IN_DELETE, "DELETE"),
                (IN_DELETE_SELF, "DELETE_SELF"),
                (IN_MOVE_SELF, "MOVE_SELF"),
                (IN_UNMOUNT, "UNMOUNT"),
                (IN_Q_OVERFLOW, "Q_OVERFLOW"),
                (IN_IGNORED, "IGNORED"),
                (IN_ISDIR, "ISDIR"),
            ];
            let mut out = String::new();
            for (bit, name) in NAMES {
                if self.mask & bit != 0 {
                    if !out.is_empty() {
                        out.push('+');
                    }
                    out.push_str(name);
                }
            }
            if out.is_empty() {
                out.push_str("?");
            }
            out
        }
    }

    impl Inotify {
        pub fn new(nonblock: bool) -> Result<Self> {
            let mut flags = libc::IN_CLOEXEC;
            if nonblock {
                flags |= libc::IN_NONBLOCK;
            }
            // SAFETY: inotify_init1 takes an int of valid IN_* flags and
            // returns an fd we take ownership of.
            let fd = unsafe { libc::inotify_init1(flags) };
            if fd < 0 {
                return Err(Error::sys("inotify_init1"));
            }
            // SAFETY: fd is a freshly created, uniquely owned descriptor.
            Ok(Inotify {
                fd: unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) },
            })
        }

        pub fn add_watch(&self, path: &Path, mask: u32) -> Result<i32> {
            let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
                Error::Parse {
                    what: "path",
                    detail: "embedded NUL".into(),
                }
            })?;
            // SAFETY: c is a valid NUL-terminated string; mask is a bitfield of
            // IN_* constants. Returns a wd or -1.
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), mask) };
            if wd < 0 {
                return Err(Error::sys("inotify_add_watch"));
            }
            Ok(wd)
        }

        pub fn rm_watch(&self, wd: i32) -> Result<()> {
            // SAFETY: plain ioctl-style call on our own fd.
            let r = unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
            if r < 0 {
                return Err(Error::sys("inotify_rm_watch"));
            }
            Ok(())
        }

        pub fn as_raw_fd(&self) -> i32 {
            self.fd.as_raw_fd()
        }

        /// Wait up to `timeout_ms` for readability. Returns false on timeout.
        pub fn wait(&self, timeout_ms: i32) -> Result<bool> {
            let mut pfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd on the stack.
            let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if r < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    return Ok(false);
                }
                return Err(Error::sys("poll"));
            }
            Ok(r > 0)
        }

        /// Drain everything currently queued, using a caller-sized buffer.
        ///
        /// The buffer size is a parameter because it changes *whether* overflow
        /// happens: a 1 MiB buffer drains ~21845 minimal events per read, a
        /// 4 KiB buffer only ~85. Any overflow claim must state the buffer.
        pub fn read(&self, buf: &mut [u8]) -> Result<Vec<Event>> {
            // SAFETY: buf is a valid writable slice of reported length.
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EAGAIN) => return Ok(Vec::new()),
                    Some(libc::EINTR) => return Ok(Vec::new()),
                    _ => return Err(Error::sys("read(inotify)")),
                }
            }
            let n = n as usize;
            let header = std::mem::size_of::<libc::inotify_event>();
            let mut events = Vec::new();
            let mut off = 0usize;
            while off + header <= n {
                // SAFETY: we checked that a full header fits before `n`.
                // read_unaligned because the buffer has no alignment guarantee.
                let ev: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf[off..].as_ptr().cast()) };
                let name_start = off + header;
                let name_end = name_start + ev.len as usize;
                let name = if ev.len == 0 || name_end > n {
                    String::new()
                } else {
                    let raw = &buf[name_start..name_end];
                    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                    String::from_utf8_lossy(&raw[..end]).into_owned()
                };
                events.push(Event {
                    wd: ev.wd,
                    mask: ev.mask,
                    cookie: ev.cookie,
                    name,
                });
                off = name_end;
            }
            Ok(events)
        }
    }
}

// ---------------------------------------------------------------------------
// NETLINK_CONNECTOR / proc events
// ---------------------------------------------------------------------------

pub mod connector {
    use super::{Error, Result};
    use std::io;
    use std::mem;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    pub const PROC_EVENT_NONE: u32 = 0x0000_0000;
    pub const PROC_EVENT_FORK: u32 = 0x0000_0001;
    pub const PROC_EVENT_EXEC: u32 = 0x0000_0002;
    pub const PROC_EVENT_EXIT: u32 = 0x8000_0000;

    const CN_IDX_PROC: u32 = 1;
    const CN_VAL_PROC: u32 = 1;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Kind {
        Fork,
        Exec,
        Exit,
        Other(u32),
    }

    #[derive(Clone, Copy, Debug)]
    pub struct Event {
        pub kind: Kind,
        /// The pid the event is about. For fork: child pid; for exec/exit: the
        /// process pid.
        pub pid: i32,
        pub tgid: i32,
        pub parent_pid: i32,
        /// Kernel timestamp, ns since boot.
        pub timestamp_ns: u64,
    }

    pub struct Client {
        fd: OwnedFd,
    }

    impl Client {
        /// Open, bind to the proc connector multicast group, and subscribe.
        ///
        /// Failure modes matter for the report: `EPERM` if the netns has no
        /// connector, `ECONNREFUSED` from `sendto` if the kernel has no proc
        /// connector registered (e.g. inside a fresh netns without one).
        pub fn open() -> Result<Self> {
            // SAFETY: standard socket(2) call; returns an fd or -1.
            let fd = unsafe {
                libc::socket(
                    libc::AF_NETLINK,
                    libc::SOCK_DGRAM,
                    libc::NETLINK_CONNECTOR,
                )
            };
            if fd < 0 {
                return Err(Error::sys("socket(NETLINK_CONNECTOR)"));
            }
            // SAFETY: fd is freshly created and uniquely owned.
            let fd = unsafe { <OwnedFd as FromRawFd>::from_raw_fd(fd) };

            let mut sa: libc::sockaddr_nl = unsafe { mem::zeroed() };
            sa.nl_family = libc::AF_NETLINK as u16;
            sa.nl_pid = std::process::id();
            sa.nl_groups = CN_IDX_PROC;
            // SAFETY: sa is initialised; length matches its type.
            let r = unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&sa as *const libc::sockaddr_nl).cast(),
                    mem::size_of::<libc::sockaddr_nl>() as u32,
                )
            };
            if r < 0 {
                return Err(Error::sys("bind(netlink)"));
            }
            let client = Client { fd };
            client.set_listening(true)?;
            Ok(client)
        }

        fn set_listening(&self, listen: bool) -> Result<()> {
            const NLMSG_HDR: usize = 16;
            const CN_MSG: usize = 20;
            const PAYLOAD: usize = 8; // struct proc_input { op, event_type }
            let total = NLMSG_HDR + CN_MSG + PAYLOAD;
            let mut buf = vec![0u8; total];

            let len = total as u32;
            let nl_type: u16 = 0x10; // NLMSG_DONE is the connector's "to kernel"
            buf[0..4].copy_from_slice(&len.to_ne_bytes());
            buf[4..6].copy_from_slice(&nl_type.to_ne_bytes());
            buf[6..8].copy_from_slice(&0u16.to_ne_bytes()); // flags
            buf[8..12].copy_from_slice(&1u32.to_ne_bytes()); // seq
            buf[12..16].copy_from_slice(&0u32.to_ne_bytes()); // pid = kernel

            let c = NLMSG_HDR;
            buf[c..c + 4].copy_from_slice(&CN_IDX_PROC.to_ne_bytes());
            buf[c + 4..c + 8].copy_from_slice(&CN_VAL_PROC.to_ne_bytes());
            buf[c + 8..c + 12].copy_from_slice(&1u32.to_ne_bytes()); // seq
            buf[c + 12..c + 16].copy_from_slice(&0u32.to_ne_bytes()); // ack
            buf[c + 16..c + 18].copy_from_slice(&(PAYLOAD as u16).to_ne_bytes());
            buf[c + 18..c + 20].copy_from_slice(&0u16.to_ne_bytes()); // flags

            let p = c + CN_MSG;
            let op: u32 = if listen { 1 } else { 2 }; // PROC_CN_MCAST_LISTEN / IGNORE
            buf[p..p + 4].copy_from_slice(&op.to_ne_bytes());
            // PROC_EVENT_ALL
            let all: u32 = 0x0000_0001
                | 0x0000_0002
                | 0x0000_0004
                | 0x0000_0040
                | 0x0000_0080
                | 0x0000_0100
                | 0x0000_0200
                | 0x2000_0000
                | 0x4000_0000
                | 0x8000_0000;
            buf[p + 4..p + 8].copy_from_slice(&all.to_ne_bytes());

            let mut dst: libc::sockaddr_nl = unsafe { mem::zeroed() };
            dst.nl_family = libc::AF_NETLINK as u16;
            // SAFETY: buf and dst are valid for the durations of the call.
            let s = unsafe {
                libc::sendto(
                    self.fd.as_raw_fd(),
                    buf.as_ptr().cast(),
                    total,
                    0,
                    (&dst as *const libc::sockaddr_nl).cast(),
                    mem::size_of::<libc::sockaddr_nl>() as u32,
                )
            };
            if s < 0 {
                return Err(Error::sys("sendto(connector)"));
            }
            Ok(())
        }

        /// Receive up to `timeout_ms`, returning every process event decoded.
        pub fn recv(&self, timeout_ms: i32) -> Result<Vec<Event>> {
            let mut pfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: single valid pollfd.
            let pr = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if pr < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    return Ok(Vec::new());
                }
                return Err(Error::sys("poll(connector)"));
            }
            if pr == 0 {
                return Ok(Vec::new());
            }
            let mut buf = vec![0u8; 1 << 16];
            // SAFETY: buf is a valid writable buffer.
            let n = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EAGAIN) | Some(libc::EINTR) => return Ok(Vec::new()),
                    _ => return Err(Error::sys("recv(connector)")),
                }
            }
            Ok(decode(&buf[..n as usize]))
        }
    }

    fn be32(b: &[u8], at: usize) -> u32 {
        let mut v = [0u8; 4];
        v.copy_from_slice(&b[at..at + 4]);
        u32::from_ne_bytes(v)
    }

    fn be64(b: &[u8], at: usize) -> u64 {
        let mut v = [0u8; 8];
        v.copy_from_slice(&b[at..at + 8]);
        u64::from_ne_bytes(v)
    }

    fn decode(buf: &[u8]) -> Vec<Event> {
        const NLMSG_HDR: usize = 16;
        const CN_MSG: usize = 20;
        let mut out = Vec::new();
        let mut off = 0usize;
        while off + NLMSG_HDR <= buf.len() {
            let nl_len = be32(buf, off) as usize;
            if nl_len < NLMSG_HDR || off + nl_len > buf.len() {
                break;
            }
            let mut p = off + NLMSG_HDR;
            let end = off + nl_len;
            while p + CN_MSG <= end {
                let payload_len = u16::from_ne_bytes([buf[p + 16], buf[p + 17]]) as usize;
                let eq = p + CN_MSG;
                if payload_len < 16 || eq + payload_len > end {
                    break;
                }
                // struct proc_event { u32 what; u32 cpu; u64 ts; union {...} }
                let what = be32(buf, eq);
                let ts = be64(buf, eq + 8);
                let u = eq + 16;
                let ev = match what {
                    PROC_EVENT_FORK => {
                        if u + 16 <= eq + payload_len {
                            Some(Event {
                                kind: Kind::Fork,
                                parent_pid: be32(buf, u) as i32,
                                pid: be32(buf, u + 8) as i32,
                                tgid: be32(buf, u + 12) as i32,
                                timestamp_ns: ts,
                            })
                        } else {
                            None
                        }
                    }
                    PROC_EVENT_EXEC => {
                        if u + 8 <= eq + payload_len {
                            Some(Event {
                                kind: Kind::Exec,
                                pid: be32(buf, u) as i32,
                                tgid: be32(buf, u + 4) as i32,
                                parent_pid: 0,
                                timestamp_ns: ts,
                            })
                        } else {
                            None
                        }
                    }
                    PROC_EVENT_EXIT => {
                        if u + 8 <= eq + payload_len {
                            Some(Event {
                                kind: Kind::Exit,
                                pid: be32(buf, u) as i32,
                                tgid: be32(buf, u + 4) as i32,
                                parent_pid: 0,
                                timestamp_ns: ts,
                            })
                        } else {
                            None
                        }
                    }
                    other => Some(Event {
                        kind: Kind::Other(other),
                        pid: 0,
                        tgid: 0,
                        parent_pid: 0,
                        timestamp_ns: ts,
                    }),
                };
                if let Some(ev) = ev {
                    if ev.kind != Kind::Other(PROC_EVENT_NONE) {
                        out.push(ev);
                    }
                }
                p = eq + payload_len;
                // cn_msg payloads are aligned to 4 bytes.
                p = (p + 3) & !3;
            }
            off = (off + nl_len + 3) & !3;
        }
        out
    }
}
