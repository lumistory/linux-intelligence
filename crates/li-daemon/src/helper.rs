//! Hardened, zero-exec privilege helper core (Phase 3: P3-06 .. P3-09).
//!
//! Enforces `security-boundary-v0.md` §2–§5:
//! - Zero `execve` calls after startup (no `std::process::Command` anywhere).
//! - `PR_SET_NO_NEW_PRIVS` enabled at startup.
//! - Closed numeric opcode allowlist + strict per-opcode argument allowlist.
//! - Lexical path whitelist + kernel `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)`.
//! - Per-connection server-minted capability token + `SO_PEERCRED` UID allowlist.
//! - 64 KiB frame cap, 8 MiB discard cap, 2 s partial-frame timeout (P3-08 / 0-G T7),
//!   64-connection semaphore, and dense single-mutex `O_APPEND` `0600` audit log.
//! - P3-09 honesty note: mode `0600` protects against other users, NOT against the owning user.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use li_journal::clock;
use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

pub const HELPER_MAX_FRAME_BYTES: usize = 64 * 1024;
pub const HELPER_DISCARD_CAP_BYTES: usize = 8 * 1024 * 1024;
pub const HELPER_MAX_PATH_BYTES: usize = 1024;
pub const HELPER_MAX_ARG_BYTES: usize = 4096;
pub const HELPER_MAX_READ_BYTES: u64 = 4096;
pub const HELPER_MAX_CONCURRENT_CLIENTS: usize = 64;
pub const HELPER_PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_millis(2_000);

pub const OPCODE_STAT_PATH: u64 = 1;
pub const OPCODE_READ_BOUNDED: u64 = 2;
pub const OPCODE_DESCRIBE: u64 = 3;
pub const OPCODE_BTRFS_SNAPSHOT: u64 = 4;

pub const CODE_BAD_FRAME: &str = "bad_frame";
pub const CODE_FRAME_TOO_LARGE: &str = "frame_too_large";
pub const CODE_DISCARD_EXCEEDED: &str = "discard_cap_exceeded";
pub const CODE_PARTIAL_TIMEOUT: &str = "partial_frame_timeout";
pub const CODE_UNKNOWN_OPCODE: &str = "unknown_opcode";
pub const CODE_BAD_OPCODE: &str = "bad_opcode";
pub const CODE_PATH_ESCAPE: &str = "path_escape";
pub const CODE_PATH_INVALID: &str = "path_invalid";
pub const CODE_ARG_TOO_LARGE: &str = "arg_too_large";
pub const CODE_ARG_MISSING: &str = "arg_missing";
pub const CODE_ARG_UNEXPECTED: &str = "arg_unexpected";
pub const CODE_NOT_AUTHORIZED: &str = "not_authorized";
pub const CODE_IO_ERROR: &str = "io_error";
pub const CODE_NOT_IMPLEMENTED: &str = "not_implemented_by_design";

#[derive(Debug, Deserialize)]
pub struct HelperRequest {
    #[serde(default)]
    pub opcode: Value,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct HelperResponse {
    pub ok: bool,
    pub opcode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl HelperResponse {
    fn ok(opcode: &str, data: Value) -> Self {
        Self {
            ok: true,
            opcode: opcode.to_owned(),
            data: Some(data),
            code: None,
            message: None,
        }
    }

    fn err(opcode: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            opcode: opcode.to_owned(),
            data: None,
            code: Some(code.to_owned()),
            message: Some(message.into()),
        }
    }
}

#[derive(Debug)]
pub enum PathReject {
    Escape,
    Invalid,
    Io(io::Error),
}

impl PathReject {
    pub fn code(&self) -> &'static str {
        match self {
            PathReject::Escape => CODE_PATH_ESCAPE,
            PathReject::Invalid => CODE_PATH_INVALID,
            PathReject::Io(_) => CODE_IO_ERROR,
        }
    }

    pub fn message(&self) -> String {
        match self {
            PathReject::Escape => "path escapes the permitted root".into(),
            PathReject::Invalid => "path rejected by lexical whitelist".into(),
            PathReject::Io(e) => format!("io: {e}"),
        }
    }
}

/// Lexical path whitelist (`[A-Za-z0-9._/-]`, no leading `/`, no `.` or `..` components).
pub fn validate_rel(raw: &str) -> Result<&str, PathReject> {
    if raw.is_empty() || raw.len() > HELPER_MAX_PATH_BYTES {
        return Err(PathReject::Invalid);
    }
    if raw.as_bytes().contains(&0) {
        return Err(PathReject::Invalid);
    }
    if raw.starts_with('/') {
        return Err(PathReject::Escape);
    }
    if raw == "." {
        return Ok(raw);
    }
    for b in raw.bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/');
        if !ok {
            return Err(PathReject::Invalid);
        }
    }
    for comp in raw.split('/') {
        if comp.is_empty() || comp == "." {
            return Err(PathReject::Invalid);
        }
        if comp == ".." {
            return Err(PathReject::Escape);
        }
    }
    Ok(raw)
}

/// Confined root handle using `rustix::fs::openat2` with strict kernel flags (0 `unsafe`).
pub struct ConfinedRoot {
    dir: File,
    path: PathBuf,
}

impl ConfinedRoot {
    pub fn open(path: &Path) -> io::Result<Self> {
        let dir = File::open(path)?;
        Ok(Self {
            dir,
            path: path.to_path_buf(),
        })
    }

    pub fn display(&self) -> String {
        self.path.display().to_string()
    }

    pub fn open_child(&self, rel: &str, oflags: OFlags) -> Result<OwnedFd, PathReject> {
        validate_rel(rel)?;
        if rel == "." {
            let cloned = self.dir.try_clone().map_err(PathReject::Io)?;
            return Ok(OwnedFd::from(cloned));
        }
        let resolve =
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        match openat2(
            &self.dir,
            rel,
            oflags | OFlags::CLOEXEC,
            Mode::empty(),
            resolve,
        ) {
            Ok(fd) => Ok(fd),
            Err(rustix::io::Errno::XDEV) | Err(rustix::io::Errno::LOOP) => Err(PathReject::Escape),
            Err(e) => Err(PathReject::Io(io::Error::from(e))),
        }
    }
}

/// Dense, ordered `0600` JSONL audit log (`O_APPEND` + mutex-coupled sequence).
pub struct AuditLog {
    file: Mutex<File>,
    seq: AtomicU64,
    path: PathBuf,
}

impl AuditLog {
    pub const MODE: u32 = 0o600;

    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(Self::MODE)
            .open(path)?;
        let md = file.metadata()?;
        if md.permissions().mode() & 0o777 != Self::MODE {
            file.set_permissions(std::fs::Permissions::from_mode(Self::MODE))?;
        }
        Ok(Self {
            file: Mutex::new(file),
            seq: AtomicU64::new(0),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Note (P3-09): `0600` protects against other users, not against the owning uid.
    pub fn record(&self, mut fields: Value) -> io::Result<u64> {
        let obj = fields
            .as_object_mut()
            .ok_or_else(|| io::Error::other("audit entry must be JSON object"))?;
        obj.insert("schema_version".into(), json!(1));
        obj.entry("ts_wall")
            .or_insert_with(|| json!(clock::wall_now_rfc3339_ms()));
        obj.entry("ts_mono_ns")
            .or_insert_with(|| json!(clock::mono_ns().unwrap_or(0)));
        obj.entry("tamper_protection")
            .or_insert_with(|| json!("mode_0600_owner_not_tamper_proof"));

        let mut guard = self
            .file
            .lock()
            .map_err(|_| io::Error::other("audit mutex poisoned"))?;
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        obj.insert("sequence".into(), json!(seq));
        let mut line = serde_json::to_vec(&fields).map_err(|e| io::Error::other(e.to_string()))?;
        line.push(b'\n');
        guard.write_all(&line)?;
        Ok(seq)
    }
}

pub fn enable_no_new_privs() -> io::Result<()> {
    rustix::thread::set_no_new_privs(true).map_err(io::Error::from)
}

pub struct HelperState {
    pub root: ConfinedRoot,
    pub audit: AuditLog,
    pub allowed_uid: u32,
    conn_seq: AtomicU64,
}

impl HelperState {
    pub fn new(root: ConfinedRoot, audit: AuditLog, allowed_uid: u32) -> Self {
        let _ = audit.record(json!({
            "event": "HelperStarted",
            "root": root.display(),
            "allowed_uid": allowed_uid,
        }));
        Self {
            root,
            audit,
            allowed_uid,
            conn_seq: AtomicU64::new(0),
        }
    }

    fn mint_token(&self) -> String {
        let n = self.conn_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mono = clock::mono_ns().unwrap_or(0);
        clock::fnv1a_hex(format!("{}-{n}-{mono}", std::process::id()).as_bytes())
    }
}

fn check_allowed_keys(
    obj: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), String> {
    for k in obj.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(k.clone());
        }
    }
    Ok(())
}

fn require_str_arg<'a>(
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, (&'static str, String)> {
    let Some(val) = obj.get(key) else {
        return Err((CODE_ARG_MISSING, format!("missing required arg `{key}`")));
    };
    let Some(s) = val.as_str() else {
        return Err((CODE_BAD_FRAME, format!("arg `{key}` must be a string")));
    };
    if s.len() > HELPER_MAX_ARG_BYTES {
        return Err((
            CODE_ARG_TOO_LARGE,
            format!("arg `{key}` exceeds {HELPER_MAX_ARG_BYTES} bytes"),
        ));
    }
    Ok(s)
}

pub fn dispatch_request(state: &HelperState, line: &[u8]) -> HelperResponse {
    let req: HelperRequest = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            return HelperResponse::err("invalid", CODE_BAD_FRAME, format!("invalid json: {e}"));
        }
    };

    let Some(opcode) = req.opcode.as_u64() else {
        return HelperResponse::err(
            &req.opcode.to_string(),
            CODE_BAD_OPCODE,
            "opcode must be an unsigned integer",
        );
    };
    let op_name = opcode.to_string();

    let args_obj = match &req.args {
        Value::Null => serde_json::Map::new(),
        Value::Object(m) => m.clone(),
        _ => return HelperResponse::err(&op_name, CODE_BAD_FRAME, "args must be a JSON object"),
    };

    match opcode {
        OPCODE_STAT_PATH => {
            if let Err(unexpected) = check_allowed_keys(&args_obj, &["rel"]) {
                return HelperResponse::err(
                    &op_name,
                    CODE_ARG_UNEXPECTED,
                    format!("unexpected argument `{unexpected}`"),
                );
            }
            let rel = match require_str_arg(&args_obj, "rel") {
                Ok(r) => r,
                Err((code, msg)) => return HelperResponse::err(&op_name, code, msg),
            };
            match state.root.open_child(rel, OFlags::PATH) {
                Ok(fd) => {
                    let f = File::from(fd);
                    match f.metadata() {
                        Ok(md) => HelperResponse::ok(
                            &op_name,
                            json!({
                                "resolved_under_root": true,
                                "kind": if md.is_dir() { "dir" } else if md.is_file() { "file" } else { "other" },
                                "size": md.size(),
                                "mode": format!("{:04o}", md.mode() & 0o7777),
                                "uid": md.uid(),
                                "gid": md.gid(),
                                "dev": md.dev(),
                                "ino": md.ino(),
                            }),
                        ),
                        Err(e) => HelperResponse::err(&op_name, CODE_IO_ERROR, e.to_string()),
                    }
                }
                Err(e) => HelperResponse::err(&op_name, e.code(), e.message()),
            }
        }
        OPCODE_READ_BOUNDED => {
            if let Err(unexpected) = check_allowed_keys(&args_obj, &["rel", "max_bytes"]) {
                return HelperResponse::err(
                    &op_name,
                    CODE_ARG_UNEXPECTED,
                    format!("unexpected argument `{unexpected}`"),
                );
            }
            let rel = match require_str_arg(&args_obj, "rel") {
                Ok(r) => r,
                Err((code, msg)) => return HelperResponse::err(&op_name, code, msg),
            };
            let max_bytes = args_obj
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(HELPER_MAX_READ_BYTES)
                .min(HELPER_MAX_READ_BYTES);
            match state.root.open_child(rel, OFlags::RDONLY) {
                Ok(fd) => {
                    let f = File::from(fd);
                    let mut buf = Vec::new();
                    match f.take(max_bytes).read_to_end(&mut buf) {
                        Ok(_) => HelperResponse::ok(
                            &op_name,
                            json!({
                                "bytes": buf.len(),
                                "truncated": buf.len() as u64 == max_bytes,
                                "preview_hex": buf.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                            }),
                        ),
                        Err(e) => HelperResponse::err(&op_name, CODE_IO_ERROR, e.to_string()),
                    }
                }
                Err(e) => HelperResponse::err(&op_name, e.code(), e.message()),
            }
        }
        OPCODE_DESCRIBE => {
            if let Err(unexpected) = check_allowed_keys(&args_obj, &[]) {
                return HelperResponse::err(
                    &op_name,
                    CODE_ARG_UNEXPECTED,
                    format!("unexpected argument `{unexpected}`"),
                );
            }
            HelperResponse::ok(
                &op_name,
                json!({
                    "root": state.root.display(),
                    "opcodes": [OPCODE_STAT_PATH, OPCODE_READ_BOUNDED, OPCODE_DESCRIBE],
                    "execve_after_startup": 0,
                }),
            )
        }
        OPCODE_BTRFS_SNAPSHOT => HelperResponse::err(
            &op_name,
            CODE_NOT_IMPLEMENTED,
            "BtrfsSnapshot via helper is disabled by design; use explicit sandboxed provider",
        ),
        _ => HelperResponse::err(&op_name, CODE_UNKNOWN_OPCODE, "unknown numeric opcode"),
    }
}

enum HelperFrame {
    Line(Vec<u8>),
    Eof,
    TooLong { fatal: bool, bytes: usize },
    PartialTimeout,
}

async fn read_helper_frame(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> io::Result<HelperFrame> {
    let mut buf = Vec::with_capacity(256);
    loop {
        let chunk = if buf.is_empty() {
            reader.fill_buf().await?
        } else {
            match tokio::time::timeout(HELPER_PARTIAL_FRAME_TIMEOUT, reader.fill_buf()).await {
                Ok(res) => res?,
                Err(_) => return Ok(HelperFrame::PartialTimeout),
            }
        };

        if chunk.is_empty() {
            return Ok(if buf.is_empty() {
                HelperFrame::Eof
            } else {
                HelperFrame::PartialTimeout
            });
        }

        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            let total = buf.len() + pos;
            if total > HELPER_MAX_FRAME_BYTES {
                reader.consume(pos + 1);
                return Ok(HelperFrame::TooLong {
                    fatal: total > HELPER_DISCARD_CAP_BYTES,
                    bytes: total,
                });
            }
            buf.extend_from_slice(&chunk[..pos]);
            reader.consume(pos + 1);
            return Ok(HelperFrame::Line(buf));
        }

        let len = chunk.len();
        if buf.len() + len > HELPER_MAX_FRAME_BYTES {
            let mut discarded = buf.len() + len;
            buf.clear();
            reader.consume(len);
            loop {
                let rest =
                    match tokio::time::timeout(HELPER_PARTIAL_FRAME_TIMEOUT, reader.fill_buf())
                        .await
                    {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => return Err(e),
                        Err(_) => {
                            return Ok(HelperFrame::TooLong {
                                fatal: true,
                                bytes: discarded,
                            });
                        }
                    };
                if rest.is_empty() {
                    return Ok(HelperFrame::TooLong {
                        fatal: true,
                        bytes: discarded,
                    });
                }
                if let Some(pos) = rest.iter().position(|&b| b == b'\n') {
                    discarded += pos;
                    reader.consume(pos + 1);
                    return Ok(HelperFrame::TooLong {
                        fatal: discarded > HELPER_DISCARD_CAP_BYTES,
                        bytes: discarded,
                    });
                }
                discarded += rest.len();
                let rlen = rest.len();
                reader.consume(rlen);
                if discarded > HELPER_DISCARD_CAP_BYTES {
                    return Ok(HelperFrame::TooLong {
                        fatal: true,
                        bytes: discarded,
                    });
                }
            }
        }
        buf.extend_from_slice(chunk);
        reader.consume(len);
    }
}

pub async fn serve_helper_connection(
    stream: tokio::net::UnixStream,
    state: Arc<HelperState>,
    _permit: Option<OwnedSemaphorePermit>,
) -> io::Result<()> {
    let creds = rustix::net::sockopt::socket_peercred(&stream)?;
    let peer_uid = creds.uid.as_raw();
    let peer_pid = creds.pid.as_raw_nonzero().get();
    let conn_token = state.mint_token();

    let (read_half, mut write_half) = stream.into_split();
    if peer_uid != state.allowed_uid {
        let resp = HelperResponse::err("auth", CODE_NOT_AUTHORIZED, "peer uid not allowed");
        let _ = state.audit.record(json!({
            "event": "AuthRejected",
            "conn_token": conn_token,
            "peer_uid": peer_uid,
            "peer_pid": peer_pid,
            "ok": false,
            "code": CODE_NOT_AUTHORIZED,
        }));
        if let Ok(mut b) = serde_json::to_vec(&resp) {
            b.push(b'\n');
            let _ = write_half.write_all(&b).await;
        }
        return Ok(());
    }

    let mut reader = BufReader::new(read_half);
    loop {
        match read_helper_frame(&mut reader).await? {
            HelperFrame::Eof => {
                let _ = state.audit.record(json!({
                    "event": "ConnectionClosed",
                    "conn_token": conn_token,
                    "peer_uid": peer_uid,
                    "peer_pid": peer_pid,
                }));
                return Ok(());
            }
            HelperFrame::PartialTimeout => {
                let resp = HelperResponse::err(
                    "frame",
                    CODE_PARTIAL_TIMEOUT,
                    "unterminated frame timed out",
                );
                let _ = state.audit.record(json!({
                    "event": "RequestServed",
                    "conn_token": conn_token,
                    "peer_uid": peer_uid,
                    "peer_pid": peer_pid,
                    "ok": false,
                    "code": CODE_PARTIAL_TIMEOUT,
                }));
                if let Ok(mut b) = serde_json::to_vec(&resp) {
                    b.push(b'\n');
                    let _ = write_half.write_all(&b).await;
                }
                return Ok(());
            }
            HelperFrame::TooLong { fatal, bytes } => {
                let code = if fatal {
                    CODE_DISCARD_EXCEEDED
                } else {
                    CODE_FRAME_TOO_LARGE
                };
                let resp = HelperResponse::err(
                    "frame",
                    code,
                    format!("frame exceeded limit ({bytes} bytes)"),
                );
                let _ = state.audit.record(json!({
                    "event": "RequestServed",
                    "conn_token": conn_token,
                    "peer_uid": peer_uid,
                    "peer_pid": peer_pid,
                    "ok": false,
                    "code": code,
                    "bytes": bytes,
                }));
                if let Ok(mut b) = serde_json::to_vec(&resp) {
                    b.push(b'\n');
                    let _ = write_half.write_all(&b).await;
                }
                if fatal {
                    return Ok(());
                }
            }
            HelperFrame::Line(line) => {
                if line.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                let resp = dispatch_request(&state, &line);
                let _ = state.audit.record(json!({
                    "event": "RequestServed",
                    "conn_token": conn_token,
                    "peer_uid": peer_uid,
                    "peer_pid": peer_pid,
                    "opcode": resp.opcode,
                    "ok": resp.ok,
                    "code": resp.code,
                }));
                if let Ok(mut b) = serde_json::to_vec(&resp) {
                    b.push(b'\n');
                    write_half.write_all(&b).await?;
                }
            }
        }
    }
}

pub async fn run_helper_loop(
    listener: UnixListener,
    state: Arc<HelperState>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let sem = Arc::new(Semaphore::new(HELPER_MAX_CONCURRENT_CLIENTS));
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            res = listener.accept() => {
                let (stream, _) = res?;
                let Ok(permit) = sem.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let st = Arc::clone(&state);
                tokio::spawn(async move {
                    let _ = serve_helper_connection(stream, st, Some(permit)).await;
                });
            }
        }
    }
    Ok(())
}

/// Bind a standard-library socket converted into Tokio listener after `0700`/`0600` setup.
pub fn bind_helper_socket(dir: &Path) -> io::Result<UnixListener> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let sock = dir.join("helper.sock");
    let _ = std::fs::remove_file(&sock);
    let l = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}
