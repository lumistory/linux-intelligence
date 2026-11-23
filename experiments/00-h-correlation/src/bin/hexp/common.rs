//! Shared fixtures for the logic experiments.

use corr0h::envelope::{Event, EvidenceStatus, Source};

pub type R<T> = Result<T, Box<dyn std::error::Error>>;

/// 2025-11-24T00:00:00Z in nanoseconds. Fixed so every result file is
/// byte-comparable between runs.
pub const TS0: u64 = 1_763_942_400_000_000_000;

pub fn ev(
    id: &str,
    source: Source,
    session: &str,
    seq: u64,
    ts: u64,
    ty: &str,
    payload: serde_json::Value,
) -> Event {
    let status = match source {
        Source::Proc | Source::Fs => EvidenceStatus::Observed,
        _ => EvidenceStatus::Declared,
    };
    Event::new(id, source, Some(session.to_string()), seq, ts, ty, payload, status)
}
