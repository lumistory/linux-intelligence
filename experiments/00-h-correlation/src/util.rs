//! Small deterministic utilities: PRNG, hashing, time formatting, percentiles.
//! No external crates on purpose: results must be reproducible byte-for-byte
//! across machines, and a 3-line PRNG is easier to trust than a version bump.

use std::time::{SystemTime, UNIX_EPOCH};

/// splitmix64 step.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0xDEAD_BEEF_CAFE_F00D,
        }
    }

    pub fn u64(&mut self) -> u64 {
        splitmix64(&mut self.state)
    }

    /// Uniform in `0..n` (`n == 0` yields 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.u64() % n
        }
    }

    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            lo
        } else {
            lo + self.below(hi - lo)
        }
    }

    /// True with probability `pct` percent.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 16 lowercase hex chars; used as a stable fingerprint component.
pub fn fnv_hex(s: &str) -> String {
    format!("{:016x}", fnv1a64(s.as_bytes()))
}

/// Collapse whitespace and trim: two spellings of one command must hash equal.
pub fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn now_mono_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = BASE.get_or_init(Instant::now);
    base.elapsed().as_nanos() as u64
}

pub fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn now_wall() -> String {
    iso8601_from_unix_ms(now_unix_ms())
}

pub fn iso8601_from_unix_ms(ms: u128) -> String {
    let secs = (ms / 1000) as i64;
    let milli = (ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        milli
    )
}

/// Howard Hinnant's `civil_from_days`, epoch 1970-01-01.
fn civil_from_days(z0: i64) -> (i64, u32, u32) {
    let z = z0 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + i64::from(m <= 2), m as u32, d as u32)
}

/// Nearest-rank percentile on an unsorted slice; returns the max for p>=100.
pub fn percentile(values: &[u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = ((p / 100.0) * (v.len() as f64 - 1.0)).round() as usize;
    v[rank.min(v.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_roundtrip_known_instants() {
        assert_eq!(iso8601_from_unix_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_from_unix_ms(1_763_942_400_000),
            "2025-11-24T00:00:00.000Z"
        );
        assert_eq!(
            iso8601_from_unix_ms(1_763_942_400_123),
            "2025-11-24T00:00:00.123Z"
        );
        // leap-year day: exercises the civil-date arithmetic beyond the easy case
        assert_eq!(
            iso8601_from_unix_ms(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.u64(), b.u64());
        }
    }

    #[test]
    fn normalize_collapses() {
        assert_eq!(normalize_ws("  ls   -l  "), "ls -l");
    }
}
