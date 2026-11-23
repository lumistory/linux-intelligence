//! Append-only JSONL audit log.
//!
//! Properties the report claims, and how they are obtained:
//! * **append-only** — the file is opened `O_APPEND|O_CREAT|O_WRONLY` once.
//!   Nothing ever seeks or truncates. `O_APPEND` makes the write atomic with
//!   respect to the file offset, but *not* with respect to other writers, so:
//! * **one line per record, never interleaved** — each record is serialised to
//!   a single `Vec<u8>` ending in `\n` and written with one `write_all` under a
//!   process-wide mutex, so concurrent handlers cannot produce half lines.
//! * **total order** — `sequence` is assigned inside the same critical section
//!   as the write, so the sequence order *is* the file order.
//! * **0600** — the file is created with mode 0600 and the mode is re-asserted
//!   with `fchmod` on open, so an existing world-readable file is corrected.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

pub struct AuditLog {
    file: Mutex<File>,
    seq: AtomicU64,
    path: PathBuf,
    /// Mirror every record to stderr so a test can compare counts without
    /// re-reading the file.
    pub echo: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("open audit log: {0}")]
    Open(#[source] io::Error),
    #[error("write audit log: {0}")]
    Write(#[source] io::Error),
}

impl AuditLog {
    pub const MODE: u32 = 0o600;

    pub fn open(path: &Path) -> Result<Self, AuditError> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(Self::MODE)
            .open(path)
            .map_err(AuditError::Open)?;
        let md = file.metadata().map_err(AuditError::Open)?;
        use std::os::unix::fs::PermissionsExt;
        if md.permissions().mode() & 0o777 != Self::MODE {
            file.set_permissions(std::fs::Permissions::from_mode(Self::MODE))
                .map_err(AuditError::Open)?;
        }
        Ok(Self {
            file: Mutex::new(file),
            seq: AtomicU64::new(0),
            path: path.to_path_buf(),
            echo: std::env::var_os("LI_AUDIT_ECHO").is_some(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, mut fields: Value) -> Result<u64, AuditError> {
        let obj = fields
            .as_object_mut()
            .ok_or_else(|| AuditError::Write(io::Error::other("audit record must be an object")))?;
        obj.insert("schema_version".into(), json!(1));
        obj.entry("event_id")
            .or_insert_with(|| json!(uuid::Uuid::new_v4().to_string()));
        obj.entry("ts_wall").or_insert_with(|| json!(now_wall()));
        obj.entry("ts_mono_ns")
            .or_insert_with(|| json!(now_mono_ns()));

        let mut guard = self
            .file
            .lock()
            .map_err(|_| AuditError::Write(io::Error::other("audit mutex poisoned")))?;
        // Sequence is assigned inside the lock, immediately before the write,
        // so sequence order and file order are the same thing.
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        obj.insert("sequence".into(), json!(seq));
        let mut line = serde_json::to_vec(&fields)
            .map_err(|e| AuditError::Write(io::Error::other(e.to_string())))?;
        line.push(b'\n');
        guard.write_all(&line).map_err(AuditError::Write)?;
        if self.echo {
            let _ = io::stderr().write_all(&line);
        }
        Ok(seq)
    }
}

pub fn now_wall() -> String {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_REALTIME and a valid timespec pointer.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    if rc != 0 {
        return "unknown".into();
    }
    let (secs, nanos) = (ts.tv_sec, ts.tv_nsec as u32);
    // Civil date from Unix seconds (Howard Hinnant's algorithm), no chrono dep.
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{nanos:09}Z")
}

pub fn now_mono_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_MONOTONIC and a valid timespec pointer.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}
