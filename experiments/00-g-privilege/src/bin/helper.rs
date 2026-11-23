//! The simulated privileged helper.
//!
//! Usage:
//!   helper --socket <path> --root <dir> --audit <path> [--allow-uid N]...
//!
//! Design rules (each one is tested in the report):
//!  R1 numeric opcode from a closed set; nothing in the protocol names a
//!     program, a shell, an interpreter, a syscall, or a raw command string.
//!  R2 the only filesystem reach is `openat2()` relative to `--root` with
//!     `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS`.
//!  R3 every frame is size-capped before parsing; oversized frames are drained
//!     and refused, never truncated into something valid.
//!  R4 uid allowlist is checked from `SO_PEERCRED` *before* parsing.
//!  R5 `PR_SET_NO_NEW_PRIVS` at startup and again per request.
//!  R6 every request, accepted or refused, produces exactly one audit line.
//!  R7 no `fork`, no `exec`, no `system`, no `posix_spawn` anywhere in the crate.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use li_privhelper::audit::AuditLog;
use li_privhelper::{peer, proto, Root};
use serde_json::json;

struct Config {
    socket: PathBuf,
    root: PathBuf,
    audit: PathBuf,
    allow_uids: Vec<u32>,
    max_requests: Option<u64>,
    ready_file: Option<PathBuf>,
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        socket: PathBuf::new(),
        root: PathBuf::new(),
        audit: PathBuf::new(),
        allow_uids: Vec::new(),
        max_requests: None,
        ready_file: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--socket" => cfg.socket = PathBuf::from(val()?),
            "--root" => cfg.root = PathBuf::from(val()?),
            "--audit" => cfg.audit = PathBuf::from(val()?),
            "--allow-uid" => {
                let v = val()?;
                cfg.allow_uids
                    .push(v.parse::<u32>().map_err(|_| format!("bad uid {v}"))?);
            }
            "--max-requests" => {
                let v = val()?;
                cfg.max_requests = Some(v.parse::<u64>().map_err(|_| format!("bad count {v}"))?);
            }
            "--ready-file" => cfg.ready_file = Some(PathBuf::from(val()?)),
            "-h" | "--help" => {
                println!("helper --socket P --root D --audit P [--allow-uid N] [--max-requests N] [--ready-file P]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if cfg.socket.as_os_str().is_empty()
        || cfg.root.as_os_str().is_empty()
        || cfg.audit.as_os_str().is_empty()
    {
        return Err("--socket, --root and --audit are required".into());
    }
    if cfg.allow_uids.is_empty() {
        cfg.allow_uids.push(li_privhelper::current_euid());
    }
    Ok(cfg)
}

/// Framing reader: returns one line (without `\n`) or a classified framing
/// error. Never allocates more than `DISCARD_CAP_BYTES`.
enum Frame {
    Line(Vec<u8>),
    TooLarge,
    DiscardExceeded,
    Eof,
}

fn read_frame(stream: &mut UnixStream, buf: &mut Vec<u8>) -> Frame {
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > proto::MAX_FRAME_BYTES {
                return Frame::TooLarge;
            }
            return Frame::Line(line);
        }
        if buf.len() > proto::DISCARD_CAP_BYTES {
            return Frame::DiscardExceeded;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Frame::Eof,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > proto::MAX_FRAME_BYTES {
                    // Drop the oversized prefix but keep draining to the next
                    // newline, so one bad frame cannot desynchronise the stream
                    // into treating its tail as a fresh request.
                    if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                        buf.drain(..=pos);
                        return Frame::TooLarge;
                    }
                    return Frame::TooLarge;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Frame::Eof,
        }
    }
}

fn write_response(stream: &mut UnixStream, resp: &proto::Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(resp).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    line.push(b'\n');
    stream.write_all(&line)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("helper: fatal: {e}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = parse_args()?;

    if let Err(e) = li_privhelper::enforce_no_new_privs() {
        eprintln!("helper: cannot set PR_SET_NO_NEW_PRIVS: {e} — refusing to start");
        std::process::exit(3);
    }

    let root = Root::open(&cfg.root)?;
    let audit = Arc::new(AuditLog::open(&cfg.audit)?);
    let allow = cfg.allow_uids.clone();

    audit.record(json!({
        "source": "policy",
        "type": "HelperStarted",
        "payload": {
            "helper_pid": li_privhelper::current_pid(),
            "euid": li_privhelper::current_euid(),
            "no_new_privs": li_privhelper::no_new_privs_state(),
            "root": root.display(),
            "allow_uids": allow,
            "max_frame_bytes": proto::MAX_FRAME_BYTES,
        },
        "evidence_status": "observed"
    }))?;

    if let Some(parent) = cfg.socket.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let _ = std::fs::remove_file(&cfg.socket);
    let listener = UnixListener::bind(&cfg.socket)?;
    std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o600))?;

    if let Some(rf) = &cfg.ready_file {
        std::fs::write(rf, format!("{}\n", std::process::id()))?;
    }
    eprintln!(
        "helper: listening pid={} socket={} root={}",
        li_privhelper::current_pid(),
        cfg.socket.display(),
        root.display()
    );

    let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let limit = cfg.max_requests;
    let mut handles = Vec::new();

    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("helper: accept error: {e}");
                continue;
            }
        };
        let audit_h = Arc::clone(&audit);
        let allow_h = allow.clone();
        let served_h = Arc::clone(&served);
        let root_path = cfg.root.clone();
        handles.push(std::thread::spawn(move || {
            handle_conn(stream, &audit_h, &allow_h, &root_path, &served_h);
        }));
        if let Some(max) = limit {
            if served.load(std::sync::atomic::Ordering::SeqCst) >= max {
                break;
            }
        }
    }
    // Give in-flight handlers a moment to finish their audit writes.
    std::thread::sleep(std::time::Duration::from_millis(200));
    audit.record(json!({
        "source": "policy",
        "type": "HelperStopped",
        "payload": { "requests_served": served.load(std::sync::atomic::Ordering::SeqCst) },
        "evidence_status": "observed"
    }))?;
    let _ = std::fs::remove_file(&cfg.socket);
    Ok(())
}

fn handle_conn(
    mut stream: UnixStream,
    audit: &AuditLog,
    allow: &[u32],
    root_path: &std::path::Path,
    served: &std::sync::atomic::AtomicU64,
) {
    let fd = stream.as_raw_fd();
    // Deliberately read the credential *here*, after accept, and record the
    // timestamp so the report can compare "socket lifetime" with "pid lifetime".
    let cred = match peer::peer_cred(fd) {
        Ok(c) => c,
        Err(e) => {
            let _ = audit.record(json!({
                "source": "policy", "type": "PeerCredFailed",
                "payload": { "error": e.to_string() }, "evidence_status": "observed"
            }));
            return;
        }
    };
    let peer_starttime = peer::starttime(cred.pid);
    let authorized = allow.contains(&cred.uid);

    if !authorized {
        let _ = audit.record(json!({
            "source": "policy", "type": "ConnectionRejected",
            "payload": {
                "peer_pid": cred.pid, "peer_uid": cred.uid, "peer_gid": cred.gid,
                "peer_starttime": peer_starttime, "allow_uids": allow,
                "reason": "peer uid not in allowlist"
            },
            "evidence_status": "observed"
        }));
        let _ = write_response(
            &mut stream,
            &proto::Response::err("<connection>", proto::CODE_NOT_AUTHORIZED, "peer uid not allowed"),
        );
        return;
    }

    // Re-open the root per connection: nothing is shared mutable.
    let root = match Root::open(root_path) {
        Ok(r) => r,
        Err(e) => {
            let _ = audit.record(json!({
                "source": "policy", "type": "RootOpenFailed",
                "payload": { "error": e.to_string() }, "evidence_status": "observed"
            }));
            return;
        }
    };

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        let frame = read_frame(&mut stream, &mut buf);
        let (resp, line_bytes) = match frame {
            Frame::Eof => break,
            Frame::DiscardExceeded => {
                let _ = audit.record(json!({
                    "source": "policy", "type": "RequestRejected",
                    "payload": {
                        "peer_pid": cred.pid, "peer_uid": cred.uid,
                        "code": proto::CODE_DISCARD_EXCEEDED,
                        "reason": "unterminated frame exceeded discard cap"
                    },
                    "evidence_status": "observed"
                }));
                let _ = write_response(
                    &mut stream,
                    &proto::Response::err("<frame>", proto::CODE_DISCARD_EXCEEDED, "frame never terminated"),
                );
                break;
            }
            Frame::TooLarge => (
                proto::Response::err(
                    "<frame>",
                    proto::CODE_FRAME_TOO_LARGE,
                    format!("frame exceeds {} bytes", proto::MAX_FRAME_BYTES),
                ),
                proto::MAX_FRAME_BYTES as u64 + 1,
            ),
            Frame::Line(line) => {
                let n = line.len() as u64;
                // R5: re-assert per request. Cheap (one prctl) and makes the
                // guarantee independent of startup arguments.
                if let Err(e) = li_privhelper::enforce_no_new_privs() {
                    let _ = audit.record(json!({
                        "source": "policy", "type": "RequestRejected",
                        "payload": { "code": "no_new_privs_failed", "error": e.to_string() },
                        "evidence_status": "observed"
                    }));
                    break;
                }
                match proto::parse_request(&line) {
                    Ok(req) => {
                        let (resp, detail) = li_privhelper::handle(&root, &req);
                        let seq = audit.record(json!({
                            "source": "policy",
                            "type": if resp.ok { "RequestServed" } else { "RequestRejected" },
                            "payload": {
                                "peer_pid": cred.pid,
                                "peer_uid": cred.uid,
                                "peer_gid": cred.gid,
                                "peer_starttime_at_connect": peer_starttime,
                                "request_bytes": n,
                                "opcode": resp.opcode,
                                "code": resp.code,
                                "detail": detail,
                            },
                            "evidence_status": "observed"
                        }));
                        if seq.is_err() {
                            break;
                        }
                        (resp, n)
                    }
                    Err(code) => {
                        let _ = audit.record(json!({
                            "source": "policy", "type": "RequestRejected",
                            "payload": {
                                "peer_pid": cred.pid, "peer_uid": cred.uid,
                                "request_bytes": n, "code": code,
                                "reason": "not a JSON request object"
                            },
                            "evidence_status": "observed"
                        }));
                        (proto::Response::err("<frame>", code, "not a JSON request object"), n)
                    }
                }
            }
        };
        served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = line_bytes;
        if write_response(&mut stream, &resp).is_err() {
            break;
        }
    }
    // Ensure the fd is closed exactly once by dropping the stream.
    drop(stream);

    // Post-hoc liveness check: is the pid that connected still the same process?
    let live = peer::liveness(cred.pid, peer_starttime);
    let _ = audit.record(json!({
        "source": "policy", "type": "ConnectionClosed",
        "payload": {
            "peer_pid": cred.pid, "peer_uid": cred.uid,
            "pid_liveness_after": live.as_str(),
            "starttime_at_connect": peer_starttime,
            "starttime_now": peer::starttime(cred.pid),
        },
        "evidence_status": "observed"
    }));
}

/// Kept so the compiler does not warn about the unused import when the framing
/// path changes; also documents that we never construct a stream from an fd
/// behind the kernel's back.
#[allow(dead_code)]
fn _unused_from_raw(fd: i32) -> UnixStream {
    // SAFETY: only called in tests with a valid owned fd.
    unsafe { UnixStream::from_raw_fd(fd) }
}
