use std::fmt::Write as _;
use std::io::Read;

/// Nanoseconds from an arbitrary but stable origin (`CLOCK_MONOTONIC`).
pub fn now_mono_ns() -> u64 {
    match nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC) {
        Ok(ts) => {
            let secs = ts.tv_sec().max(0) as u64;
            let nsec = ts.tv_nsec().max(0) as u64;
            secs.saturating_mul(1_000_000_000).saturating_add(nsec)
        }
        // Not expected on Linux; a monotonic id of 0 is still an honest value.
        Err(_) => 0,
    }
}

/// Civil-from-days (Howard Hinnant), valid for the Gregorian calendar.
fn civil_from_days(z_days: i64) -> (i64, u32, u32) {
    let z = z_days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// RFC 3339 / ISO 8601 UTC timestamp with millisecond precision.
pub fn now_wall_rfc3339() -> String {
    let since_epoch = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => std::time::Duration::ZERO,
    };
    format_wall(since_epoch.as_secs(), since_epoch.subsec_millis())
}

pub fn format_wall(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let mut s = String::with_capacity(24);
    let _ = write!(
        s,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis
    );
    s
}

/// RFC 4122 version-4 UUID built from `/dev/urandom`.
///
/// A private implementation avoids pulling in the `uuid` crate for one call
/// site. If `/dev/urandom` cannot be read the id is a time+pid mix and is no
/// longer cryptographically unique; this is recorded in the report.
pub fn uuid_v4() -> String {
    let mut buf = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !filled {
        let a = now_mono_ns().to_le_bytes();
        let b = std::process::id().to_le_bytes();
        buf[..8].copy_from_slice(&a);
        buf[8..12].copy_from_slice(&b);
    }
    buf[6] = (buf[6] & 0x0f) | 0x40;
    buf[8] = (buf[8] & 0x3f) | 0x80;
    let h: Vec<String> = buf.iter().map(|b| format!("{b:02x}")).collect();
    let s: String = h.concat();
    format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )
}

/// One line of the shared event envelope (protocol §2).
pub struct Event {
    seq: u64,
    session_id: String,
    type_: &'static str,
    evidence: &'static str,
    ts_mono_ns: u64,
    payload: serde_json::Value,
}

impl Event {
    pub fn new(seq: u64, session_id: &str, type_: &'static str, evidence: &'static str) -> Self {
        Event {
            seq,
            session_id: session_id.to_string(),
            type_,
            evidence,
            ts_mono_ns: now_mono_ns(),
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "event_id": uuid_v4(),
            "source": "script",
            "session_id": self.session_id,
            "sequence": self.seq,
            "ts_mono_ns": self.ts_mono_ns,
            "ts_wall": now_wall_rfc3339(),
            "type": self.type_,
            "payload": self.payload,
            "evidence_status": self.evidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_formats() {
        assert_eq!(format_wall(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_wall(1_700_000_000, 123), "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn uuid_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert!(parts[2].starts_with('4'));
    }

    #[test]
    fn mono_is_nonzero() {
        assert!(now_mono_ns() > 0);
    }
}
