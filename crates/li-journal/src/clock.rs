//! Two clocks, and an honest note about the resolution of each.
//!
//! This crate has no `libc` dependency and no `unsafe`, so `clock_gettime` is
//! not callable directly. That constraint is visible in the numbers below rather
//! than hidden.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// `CLOCK_MONOTONIC` nanoseconds since boot, as well as this process can see it.
///
/// # Resolution — do not overclaim it
///
/// `Instant` in std is `CLOCK_MONOTONIC`, but it has no defined epoch and cannot
/// be converted to nanoseconds-since-boot. The absolute anchor therefore comes
/// from `/proc/uptime`, which reports **centiseconds** — i.e. 10 ms, the same
/// `CLK_TCK` tick that 0-H §8.1.4 requires every stored cross-source order to
/// carry ±1 of. Deltas *within* this process come from `Instant` and are finer,
/// but that finer resolution is not comparable across processes, so it is not
/// claimed: the value is good to 10 ms and no better.
///
/// Returns `None` if `/proc/uptime` is unreadable (not Linux, unusual sandbox).
/// The caller decides whether a missing clock is fatal; a fabricated one never
/// is.
pub fn mono_ns() -> Option<u64> {
    struct Anchor {
        uptime_ns: u64,
        at: Instant,
    }
    static ANCHOR: OnceLock<Option<Anchor>> = OnceLock::new();
    let anchor = ANCHOR.get_or_init(|| {
        let text = std::fs::read_to_string("/proc/uptime").ok()?;
        let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
        if !secs.is_finite() || secs < 0.0 {
            return None;
        }
        Some(Anchor {
            uptime_ns: (secs * 1e9) as u64,
            at: Instant::now(),
        })
    });
    anchor.as_ref().map(|a| {
        let delta = Instant::now().saturating_duration_since(a.at);
        a.uptime_ns
            .saturating_add(u64::try_from(delta.as_nanos()).unwrap_or(u64::MAX))
    })
}

/// RFC 3339 UTC with milliseconds and a `Z`, i.e. exactly the shape
/// `li_proto::Event::ts_wall` validates (fixed 24 bytes, lexicographically
/// sortable).
pub fn wall_now_rfc3339_ms() -> String {
    wall_rfc3339_ms(SystemTime::now())
}

pub fn wall_rfc3339_ms(t: SystemTime) -> String {
    let ms = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (secs, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 -> (year, month, day). Proleptic Gregorian, the
/// algorithm from Howard Hinnant's `chrono`-compatible write-up; kept here so
/// the crate needs no date library.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 64-bit FNV-1a, hex. Used for the migration ledger and the secret digest.
///
/// 0-H's own report calls this "linkable by design, not collision-resistant
/// enough to be trusted for anything but same-value-seen-again", and that is
/// exactly the two jobs it has here: notice that a migration file changed, and
/// notice that the same credential was used twice. It is **not** a KDF and must
/// not be described as one.
pub fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_shape_matches_the_protocol_validator() {
        let ms = 1_763_942_400_000u128; // 2025-11-23T18:00:00.000Z-ish
        let t = UNIX_EPOCH + std::time::Duration::from_millis(ms as u64);
        let s = wall_rfc3339_ms(t);
        assert_eq!(s.len(), 24);
        assert!(s.ends_with('Z'));
        assert!(li_proto_shape_ok(&s), "{s}");
    }

    /// Mirrors `li_proto`'s private `check::ts_wall` without depending on it
    /// being public: the journal may not write a timestamp the protocol would
    /// reject on the way back in.
    fn li_proto_shape_ok(s: &str) -> bool {
        let b = s.as_bytes();
        const SHAPE: &[u8] = b"dddd-dd-ddTdd:dd:dd.dddZ";
        if b.len() != SHAPE.len() {
            return false;
        }
        SHAPE.iter().enumerate().all(|(i, want)| match want {
            b'd' => b[i].is_ascii_digit(),
            other => b[i] == *other,
        })
    }

    #[test]
    fn known_epochs_convert() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn fnv_is_stable_and_hex() {
        assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
        assert_eq!(fnv1a_hex(b"a").len(), 16);
        assert_ne!(fnv1a_hex(b"a"), fnv1a_hex(b"b"));
    }
}
