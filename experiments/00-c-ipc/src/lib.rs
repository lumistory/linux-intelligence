//! Shared protocol pieces for experiment 00-C.
//!
//! Transport: `AF_UNIX`/`SOCK_STREAM`, framing = one JSON object per line.
//! Envelope: `docs/00-phase0-protocol.md` §2.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u64 = 1;

/// Maximum accepted frame size (one JSON line) on the server side.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// After an oversized frame starts, the server discards input up to this many
/// bytes looking for a newline. Beyond that the connection is closed.
pub const DISCARD_CAP: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u64,
    pub event_id: String,
    pub source: String,
    pub session_id: String,
    pub sequence: u64,
    pub ts_mono_ns: u64,
    pub ts_wall: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: serde_json::Value,
    pub evidence_status: String,
}

impl Event {
    pub fn new(session: &str, sequence: u64, kind: &str, payload: serde_json::Value) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            event_id: new_event_id(),
            source: "script".to_string(),
            session_id: session.to_string(),
            sequence,
            ts_mono_ns: mono_ns(),
            ts_wall: wall_iso_now(),
            kind: kind.to_string(),
            payload,
            evidence_status: "observed".to_string(),
        }
    }
}

/// Messages the server sends back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMsg {
    pub schema_version: u64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    /// Highest contiguously-accepted sequence per session. Per-event `Ack`
    /// frames are best-effort (they may be dropped when the client does not
    /// read fast enough); this watermark is the authoritative delivery proof.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watermarks: Option<BTreeMap<String, u64>>,
    /// Number of events accepted per session. `watermark` alone cannot tell
    /// "the client deliberately dropped some" apart from "the transport lost
    /// some"; the pair can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<u64>,
}

impl ServerMsg {
    pub fn ack(event_id: &str, sequence: u64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "Ack".into(),
            event_id: Some(event_id.to_string()),
            sequence: Some(sequence),
            watermarks: None,
            counts: None,
            code: None,
            message: None,
            nonce: None,
        }
    }

    pub fn pong(
        nonce: u64,
        watermarks: BTreeMap<String, u64>,
        counts: BTreeMap<String, u64>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "Pong".into(),
            event_id: None,
            sequence: None,
            watermarks: Some(watermarks),
            counts: Some(counts),
            code: None,
            message: None,
            nonce: Some(nonce),
        }
    }

    pub fn error(code: &str, message: &str) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "Error".into(),
            event_id: None,
            sequence: None,
            watermarks: None,
            counts: None,
            code: Some(code.to_string()),
            message: Some(message.to_string()),
            nonce: None,
        }
    }
}

/// Client request. An event frame is recognised by carrying `event_id`; the
/// event's own `type` is an open string (the protocol does not enumerate event
/// kinds), so it cannot be used as a serde tag.
#[derive(Debug, Clone, Deserialize)]
pub enum ClientMsg {
    Event(Event),
    Ping { nonce: u64 },
}

pub fn parse_client_line(line: &[u8]) -> Result<ClientMsg, String> {
    let v: serde_json::Value =
        serde_json::from_slice(line).map_err(|e| format!("invalid_json: {e}"))?;
    if v.get("event_id").is_some() {
        return serde_json::from_value::<Event>(v)
            .map(ClientMsg::Event)
            .map_err(|e| format!("invalid_envelope: {e}"));
    }
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "Ping" => serde_json::from_value::<PingMsg>(v)
            .map(|p| ClientMsg::Ping { nonce: p.nonce })
            .map_err(|e| format!("invalid_ping: {e}")),
        "" => Err("missing_type".to_string()),
        other => Err(format!("unknown_frame_type: {other}")),
    }
}

#[derive(Debug, Deserialize)]
struct PingMsg {
    nonce: u64,
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// CLOCK_MONOTONIC in nanoseconds. `Instant` has no absolute epoch, and the
/// envelope wants a process-independent monotonic stamp, so this calls
/// `clock_gettime` directly.
pub fn mono_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, properly aligned `timespec` and CLOCK_MONOTONIC
    // is always supported on Linux; the call only writes into `ts`.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// RFC 3339 UTC with milliseconds, no chrono dependency.
pub fn wall_iso_now() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    let (y, mo, da, hh, mi, ss) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{da:02}T{hh:02}:{mi:02}:{ss:02}.{millis:03}Z")
}

fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mi, ss) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hh, mi, ss)
}

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

pub fn new_event_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Framing (sync side, used by clients)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ReadOutcome {
    Frame(Vec<u8>),
    /// Frame exceeded the limit; its bytes were skipped, the connection is
    /// still usable (the newline terminating it was consumed).
    TooLarge {
        bytes: u64,
    },
    Eof,
}

/// Read one `\n`-terminated frame with a hard size limit. Never allocates more
/// than `max` bytes for the frame body.
pub fn read_frame<R: BufRead>(r: &mut R, max: usize) -> io::Result<ReadOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut oversize: Option<u64> = None;
    loop {
        let available = match r.fill_buf() {
            Ok(b) => b,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(match oversize {
                Some(bytes) => ReadOutcome::TooLarge { bytes },
                None if buf.is_empty() => ReadOutcome::Eof,
                None => ReadOutcome::Frame(buf),
            });
        }
        let (used, found) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        let chunk = &available[..used];
        match oversize {
            None => {
                if buf.len() + chunk.len() > max {
                    oversize = Some((buf.len() + chunk.len()) as u64);
                    buf.clear();
                    buf.shrink_to(4096);
                } else {
                    buf.extend_from_slice(chunk);
                }
            }
            Some(ref mut n) => *n += chunk.len() as u64,
        }
        r.consume(used);
        if found {
            return Ok(match oversize {
                Some(bytes) => ReadOutcome::TooLarge { bytes },
                None => {
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                    }
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    ReadOutcome::Frame(buf)
                }
            });
        }
    }
}

pub fn write_json_line<W: Write, T: Serialize>(w: &mut W, v: &T) -> io::Result<usize> {
    let mut line = serde_json::to_vec(v).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    Ok(line.len())
}

// ---------------------------------------------------------------------------
// Peer credentials
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// `SO_PEERCRED` on a connected socket. Linux-only.
///
/// SAFETY: `getsockopt` is called with a correctly sized `ucred` out-buffer and
/// a matching length; failure is reported as an error instead of being ignored.
pub fn peer_cred<S: std::os::fd::AsRawFd>(sock: &S) -> io::Result<PeerCred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: see above; all pointers are valid for the duration of the call.
    let rc = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::other("SO_PEERCRED returned unexpected length"));
    }
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
    })
}

// ---------------------------------------------------------------------------
// Statistics helpers
// ---------------------------------------------------------------------------

pub struct Latency {
    samples: Vec<u64>,
}

impl Latency {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            samples: Vec::with_capacity(n),
        }
    }

    pub fn record(&mut self, nanos: u64) {
        self.samples.push(nanos);
    }

    pub fn extend(&mut self, other: &[u64]) {
        self.samples.extend_from_slice(other);
    }

    pub fn percentile(&mut self, p: f64) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        self.samples.sort_unstable();
        let idx = ((self.samples.len() as f64 - 1.0) * p).round() as usize;
        self.samples[idx.min(self.samples.len() - 1)]
    }

    pub fn max(&mut self) -> u64 {
        self.samples.iter().copied().max().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

pub fn micros(ns: u64) -> f64 {
    ns as f64 / 1000.0
}

pub fn millis(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

pub fn default_socket_path() -> String {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    format!("{base}/li-phase0-ipc/ipc.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf: &[u8] = b"{\"a\":1}\n{\"b\":2}\n";
        match read_frame(&mut buf, MAX_FRAME_BYTES).unwrap() {
            ReadOutcome::Frame(f) => assert_eq!(f, b"{\"a\":1}"),
            other => panic!("unexpected {other:?}"),
        }
        match read_frame(&mut buf, MAX_FRAME_BYTES).unwrap() {
            ReadOutcome::Frame(f) => assert_eq!(f, b"{\"b\":2}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            read_frame(&mut buf, MAX_FRAME_BYTES).unwrap(),
            ReadOutcome::Eof
        ));
    }

    #[test]
    fn oversized_frame_is_skipped_and_stream_resumes() {
        let big = vec![b'x'; 4096];
        let mut wire = Vec::new();
        wire.extend_from_slice(&big);
        wire.push(b'\n');
        wire.extend_from_slice(b"{\"ok\":1}\n");
        let mut cursor: &[u8] = &wire;
        match read_frame(&mut cursor, 64).unwrap() {
            ReadOutcome::TooLarge { bytes } => assert_eq!(bytes, 4097),
            other => panic!("unexpected {other:?}"),
        }
        match read_frame(&mut cursor, 64).unwrap() {
            ReadOutcome::Frame(f) => assert_eq!(f, b"{\"ok\":1}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn wall_clock_renders() {
        let s = wall_iso_now();
        assert_eq!(s.len(), 24, "{s}");
        assert!(s.ends_with('Z'));
    }
}
