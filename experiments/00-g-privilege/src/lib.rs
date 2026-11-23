//! `li-privhelper` — a *simulated* privileged helper, built only to measure
//! whether the interface shape is safe. It never performs a real privileged
//! operation; it is meant to be run inside a user namespace or as the same
//! unprivileged user, and it is used to prove that the request surface cannot
//! reach command execution.

pub mod audit;
pub mod path;
pub mod peer;
pub mod proto;

use serde_json::Value;

pub use path::{Root, PathError};

/// Fixed opcode table. **Data, not commands.** There is no opcode whose effect
/// is "run this", "eval this", or "open an arbitrary path".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    /// Report the helper's own build, uid, and no_new_privs state.
    Describe = 1,
    /// `stat()` a path relative to the helper's configured root.
    StatPath = 2,
    /// Read at most 4096 bytes of a file relative to the root.
    ReadSmall = 3,
    /// List one directory level relative to the root.
    ListDir = 4,
    /// Ask the kernel for a btrfs snapshot — always refused here, recorded so
    /// the report can show *why* the opcode exists but is not implemented.
    BtrfsSnapshot = 90,
}

impl Opcode {
    pub fn from_wire(v: &Value) -> Result<Self, OpcodeError> {
        let n = match v {
            Value::Number(n) => n.as_u64().ok_or(OpcodeError::BadType)?,
            Value::String(_) => return Err(OpcodeError::WrongType),
            _ => return Err(OpcodeError::BadType),
        };
        match n {
            1 => Ok(Opcode::Describe),
            2 => Ok(Opcode::StatPath),
            3 => Ok(Opcode::ReadSmall),
            4 => Ok(Opcode::ListDir),
            90 => Ok(Opcode::BtrfsSnapshot),
            _ => Err(OpcodeError::Unknown(n)),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Opcode::Describe => "Describe",
            Opcode::StatPath => "StatPath",
            Opcode::ReadSmall => "ReadSmall",
            Opcode::ListDir => "ListDir",
            Opcode::BtrfsSnapshot => "BtrfsSnapshot",
        }
    }

    /// Opcodes that are recognised but, by design, never implemented. Kept in
    /// the table so that a request for them is a *classified* refusal.
    pub fn is_implemented(self) -> bool {
        !matches!(self, Opcode::BtrfsSnapshot)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpcodeError {
    #[error("opcode must be a JSON number")]
    BadType,
    #[error("opcode must be numeric, not a string")]
    WrongType,
    #[error("unknown opcode {0}")]
    Unknown(u64),
}

impl OpcodeError {
    pub fn code(&self) -> &'static str {
        match self {
            OpcodeError::Unknown(_) => proto::CODE_UNKNOWN_OPCODE,
            OpcodeError::BadType | OpcodeError::WrongType => proto::CODE_BAD_OPCODE,
        }
    }
}

/// Set `PR_SET_NO_NEW_PRIVS` and fail loudly if it cannot be set. Called once at
/// startup and again before every request is handled, so that a helper which
/// was started with more privilege than intended still cannot let a request
/// acquire new privilege through a setuid binary.
pub fn enforce_no_new_privs() -> std::io::Result<()> {
    // SAFETY: prctl(PR_SET_NO_NEW_PRIVS) takes 4 integer arguments and has no
    // pointer arguments, so there is no aliasing or lifetime concern.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn no_new_privs_state() -> bool {
    // `PR_GET_NO_NEW_PRIVS` does **not** write through a pointer: the kernel
    // returns the state as the syscall's return value (1 = set, 0 = unset,
    // negative = error). Measured the hard way — the first version of this
    // function used an out-pointer and reported `false` for a process that had
    // already set the flag.
    //
    // SAFETY: prctl(PR_GET_NO_NEW_PRIVS) ignores args 2..5 and has no pointer
    // arguments.
    let rc = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
    rc == 1
}

/// Reject non-UTF-8 and oversized string arguments before any parsing.
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, &'static str> {
    let v = args.get(key).ok_or(proto::CODE_ARG_MISSING)?;
    let s = v.as_str().ok_or(proto::CODE_ARG_MISSING)?;
    if s.len() > proto::MAX_ARG_BYTES {
        return Err(proto::CODE_ARG_TOO_LARGE);
    }
    Ok(s)
}

/// Handle one already-parsed request. Returns the response and the audit record
/// payload. Pure with respect to the process: no exec, no /etc, no signals.
pub fn handle(root: &Root, req: &proto::Request) -> (proto::Response, Value) {
    let opcode = match Opcode::from_wire(&req.opcode) {
        Ok(op) => op,
        Err(e) => {
            let code = e.code();
            let resp = proto::Response::err("<none>", code, e.to_string());
            return (resp, serde_json::json!({ "outcome": "rejected", "code": code }));
        }
    };
    let name = opcode.name();

    if !opcode.is_implemented() {
        let resp = proto::Response::err(
            name,
            proto::CODE_NOT_IMPLEMENTED,
            "opcode exists in the table but is intentionally not implemented",
        );
        return (resp, serde_json::json!({ "outcome": "refused_by_design", "code": proto::CODE_NOT_IMPLEMENTED }));
    }

    match opcode {
        Opcode::Describe => {
            let data = serde_json::json!({
                "uid": current_euid(),
                "no_new_privs": no_new_privs_state(),
                "root": root.display(),
                "ops": ["Describe", "StatPath", "ReadSmall", "ListDir"],
            });
            (
                proto::Response::ok(name, data.clone()),
                serde_json::json!({ "outcome": "ok", "detail": data }),
            )
        }
        Opcode::StatPath | Opcode::ReadSmall | Opcode::ListDir => {
            let rel = match arg_str(&req.args, "path") {
                Ok(p) => p,
                Err(code) => {
                    return (
                        proto::Response::err(name, code, "missing or oversized `path`"),
                        serde_json::json!({ "outcome": "rejected", "code": code }),
                    )
                }
            };
            if let Err(e) = path::validate_rel(rel) {
                let code = e.code();
                return (
                    proto::Response::err(name, code, e.to_string()),
                    serde_json::json!({ "outcome": "rejected", "code": code, "path": rel }),
                );
            }
            let result = match opcode {
                Opcode::StatPath => path::stat_path(root, rel),
                Opcode::ReadSmall => path::read_small(root, rel, proto::MAX_READ_BYTES),
                Opcode::ListDir => list_dir(root, rel),
                _ => unreachable!("outer match restricts to the three path opcodes"),
            };
            match result {
                Ok(data) => (
                    proto::Response::ok(name, data.clone()),
                    serde_json::json!({ "outcome": "ok", "path": rel, "detail": data }),
                ),
                Err(e) => {
                    let code = e.code();
                    (
                        proto::Response::err(name, code, e.to_string()),
                        serde_json::json!({ "outcome": "rejected", "code": code, "path": rel }),
                    )
                }
            }
        }
        Opcode::BtrfsSnapshot => unreachable!("guarded by is_implemented"),
    }
}

fn list_dir(root: &Root, rel: &str) -> Result<Value, PathError> {
    let f = root.open_child(rel, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let rd = std::fs::read_dir(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&f)))?;
    let mut names: Vec<String> = Vec::new();
    for e in rd.flatten().take(1024) {
        names.push(e.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(serde_json::json!({ "count": names.len(), "names": names }))
}

pub fn current_euid() -> u32 {
    // SAFETY: geteuid has no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

pub fn current_pid() -> i32 {
    // SAFETY: getpid has no arguments and cannot fail.
    unsafe { libc::getpid() }
}
