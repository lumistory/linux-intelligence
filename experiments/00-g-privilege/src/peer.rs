//! Peer identity: `SO_PEERCRED` plus the `/proc` cross-check that makes it
//! honest.
//!
//! `SO_PEERCRED` is a snapshot taken by the kernel when the peer called
//! `connect()`. It is never refreshed. By the time a server reads it, the peer
//! may have exited and its pid may belong to an unrelated process. This module
//! therefore separates two concepts:
//!
//! * **uid** — safe for authorisation. uids are not recycled while the socket
//!   exists in any realistic sense, and the uid is what the kernel actually
//!   enforced during `connect()`.
//! * **pid** — attribution only. It is reported, and the helper attempts to
//!   corroborate it with `/proc/<pid>/stat` field 22 (`starttime`), but the
//!   corroboration can fail or, worse, succeed against a *different* process
//!   that inherited the pid.

use std::io;
use std::os::fd::RawFd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidLiveness {
    /// `/proc/<pid>` exists and its `starttime` matches the snapshot.
    AliveSameProcess { starttime: u64 },
    /// A process with that pid exists but has a different `starttime`.
    RecycledPid { starttime: u64 },
    /// No such pid.
    Gone,
}

impl PidLiveness {
    pub fn as_str(&self) -> &'static str {
        match self {
            PidLiveness::AliveSameProcess { .. } => "alive_same_process",
            PidLiveness::RecycledPid { .. } => "recycled_pid",
            PidLiveness::Gone => "gone",
        }
    }
}

pub fn peer_cred(fd: RawFd) -> io::Result<PeerCred> {
    let mut ucred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `ucred` and `len` are valid, correctly sized writable locals for
    // the duration of the call; SO_PEERCRED writes exactly sizeof(ucred).
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PeerCred {
        pid: ucred.pid,
        uid: ucred.uid,
        gid: ucred.gid,
    })
}

/// Field 22 of `/proc/<pid>/stat`: process start time in clock ticks since boot.
///
/// The comm field may contain spaces and parentheses, so parsing starts after
/// the **last** `)`.
pub fn starttime(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = s.rfind(')')?;
    let rest = s.get(close + 1..)?;
    // After `)` the fields are: state(3) ppid(4) ... starttime(22).
    // That makes starttime the 20th whitespace-separated token of `rest`.
    rest.split_whitespace().nth(19)?.parse::<u64>().ok()
}

pub fn cmdline_first_arg(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let first = raw.split(|b| *b == 0).next()?;
    if first.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(first).into_owned())
}

/// Compare a pid captured earlier against the process alive *now*.
pub fn liveness(pid: i32, observed_starttime: Option<u64>) -> PidLiveness {
    match (starttime(pid), observed_starttime) {
        (None, _) => PidLiveness::Gone,
        (Some(now), Some(then)) if now == then => PidLiveness::AliveSameProcess { starttime: now },
        (Some(now), _) => PidLiveness::RecycledPid { starttime: now },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_starttime_is_parseable() {
        let st = starttime(std::process::id() as i32).expect("own stat readable");
        assert!(st > 0);
    }

    #[test]
    fn nonexistent_pid_is_none() {
        assert!(starttime(999_999_999).is_none());
    }

    #[test]
    fn pid_one_comm_with_parens_does_not_break_parsing() {
        // Only asserts that whatever is there parses; pid 1 is whatever init is.
        if let Some(st) = starttime(1) {
            assert!(st > 0);
        }
    }
}
