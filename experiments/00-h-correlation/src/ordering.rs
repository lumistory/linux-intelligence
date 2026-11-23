//! Out-of-order stream handling.
//!
//! Two different questions, deliberately measured separately:
//!  * **order recovery** — can the consumer see facts in the order they
//!    happened?
//!  * **completeness under gaps** — when `sequence` has a hole (0-C proved the
//!    gap is exact), what does the consumer do?
//!
//! The reorderer is driven by an explicit clock (`advance(now_ns)`) rather than
//! by "time since last push", because a stalled stream must still flush: a
//! grace window keyed on arrival of *more* events never fires on a quiet
//! session, which is exactly when a lost event sits in the buffer forever.

use crate::envelope::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    /// Emitted in per-session `sequence` order.
    InOrder,
    /// Emitted out of order: `sequence` hole older than the grace window.
    ForcedGap { expected: u64, got: u64 },
    /// Buffer cap hit before the hole filled: memory bound, not correctness.
    ForcedCapacity { expected: u64, got: u64 },
    /// Arrived with a `sequence` below the watermark, i.e. it is older than
    /// something already emitted. It cannot be reordered into place without
    /// un-emitting, so it is emitted now and marked. **This is the case a naive
    /// reorderer gets wrong**: it either stalls forever or silently drops the
    /// event. It happens whenever a consumer attaches to a session mid-stream,
    /// which is the normal case after a daemon restart.
    LateArrival { watermark: u64, got: u64 },
}

impl Emit {
    pub fn is_forced(&self) -> bool {
        !matches!(self, Emit::InOrder)
    }
}

#[derive(Debug, Clone)]
pub struct Emitted {
    pub event: Event,
    pub kind: Emit,
    /// How long the event sat in the buffer, in ns, per the supplied clock.
    pub held_ns: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ReorderPolicy {
    /// Maximum events buffered per session before the smallest sequence is
    /// force-flushed. Bounds memory; bigger buffer tolerates deeper disorder.
    pub max_hold: usize,
    /// Clock distance (ns) beyond which a missing `sequence` is declared lost
    /// rather than late. **This is the out-of-order tolerance window.**
    pub grace_ns: u64,
    /// Known first `sequence` of the session. `None` means "trust the first
    /// arrival", which is what a consumer does after a restart with no
    /// checkpoint; that choice makes everything older than the first arrival a
    /// `LateArrival`.
    pub initial_sequence: Option<u64>,
}

impl Default for ReorderPolicy {
    fn default() -> Self {
        Self {
            max_hold: 256,
            grace_ns: 500_000_000,
            initial_sequence: None,
        }
    }
}

#[derive(Debug, Default)]
struct SessionState {
    next_expected: u64,
    started: bool,
    held: Vec<(Event, u64)>, // (event, arrival_ns)
    emitted_in_order: usize,
    forced_gaps: usize,
    forced_capacity: usize,
    late_arrivals: usize,
}

#[derive(Debug)]
pub struct SeqReorderer {
    policy: ReorderPolicy,
    sessions: std::collections::HashMap<String, SessionState>,
    pub max_hold_observed: usize,
}

impl SeqReorderer {
    pub fn new(policy: ReorderPolicy) -> Self {
        Self {
            policy,
            sessions: std::collections::HashMap::new(),
            max_hold_observed: 0,
        }
    }

    pub fn push(&mut self, event: Event, arrival_ns: u64) -> Vec<Emitted> {
        let session = event.session_id.clone().unwrap_or_else(|| "-".to_string());
        let seq = event.sequence;
        let st = self.sessions.entry(session).or_default();
        if !st.started {
            st.next_expected = self.policy.initial_sequence.unwrap_or(seq);
            st.started = true;
        }
        // Older than the watermark: nothing can put it back in order.
        if seq < st.next_expected {
            st.late_arrivals += 1;
            return vec![Emitted {
                event,
                kind: Emit::LateArrival {
                    watermark: st.next_expected,
                    got: seq,
                },
                held_ns: 0,
            }];
        }
        st.held.push((event, arrival_ns));
        st.held.sort_by_key(|(e, _)| e.sequence);
        self.max_hold_observed = self.max_hold_observed.max(st.held.len());
        let mut out = Vec::new();
        flush_session(st, self.policy, arrival_ns, false, &mut out);
        out
    }

    /// Wall-clock tick: flush holes older than `grace_ns`.
    pub fn advance(&mut self, now_ns: u64) -> Vec<Emitted> {
        let mut out = Vec::new();
        for st in self.sessions.values_mut() {
            flush_session(st, self.policy, now_ns, false, &mut out);
        }
        out.sort_by_key(|e| (e.event.session_id.clone(), e.event.sequence));
        out
    }

    /// End of stream: everything still held is emitted, holes marked.
    pub fn finish(&mut self) -> Vec<Emitted> {
        let mut out = Vec::new();
        for st in self.sessions.values_mut() {
            let now = st.held.iter().map(|(_, a)| *a).max().unwrap_or(0);
            flush_session(st, self.policy, now, true, &mut out);
        }
        out.sort_by_key(|e| (e.event.session_id.clone(), e.event.sequence));
        out
    }
}

fn flush_session(
    st: &mut SessionState,
    policy: ReorderPolicy,
    now_ns: u64,
    force: bool,
    out: &mut Vec<Emitted>,
) {
    loop {
        if st.held.is_empty() {
            return;
        }
        if st.held[0].0.sequence == st.next_expected {
            let (ev, arrival) = st.held.remove(0);
            st.next_expected += 1;
            st.emitted_in_order += 1;
            out.push(Emitted {
                event: ev,
                kind: Emit::InOrder,
                held_ns: now_ns.saturating_sub(arrival),
            });
            continue;
        }

        let expected = st.next_expected;
        let got = st.held[0].0.sequence;
        let oldest_arrival = st.held.iter().map(|(_, a)| *a).min().unwrap_or(now_ns);
        let waited = now_ns.saturating_sub(oldest_arrival);
        let over_capacity = st.held.len() > policy.max_hold;

        if force || over_capacity || waited >= policy.grace_ns {
            let (ev, arrival) = st.held.remove(0);
            st.next_expected = ev.sequence + 1;
            let kind = if waited >= policy.grace_ns || force {
                st.forced_gaps += 1;
                Emit::ForcedGap { expected, got }
            } else {
                st.forced_capacity += 1;
                Emit::ForcedCapacity { expected, got }
            };
            out.push(Emitted {
                event: ev,
                kind,
                held_ns: now_ns.saturating_sub(arrival),
            });
            continue;
        }
        return;
    }
}

impl SeqReorderer {
    pub fn still_held(&self) -> usize {
        self.sessions.values().map(|s| s.held.len()).sum()
    }

    pub fn stats(&self) -> ReorderStats {
        ReorderStats {
            emitted_in_order: self.sessions.values().map(|s| s.emitted_in_order).sum(),
            forced_gaps: self.sessions.values().map(|s| s.forced_gaps).sum(),
            forced_capacity: self.sessions.values().map(|s| s.forced_capacity).sum(),
            late_arrivals: self.sessions.values().map(|s| s.late_arrivals).sum(),
            max_hold_observed: self.max_hold_observed,
            still_held: self.still_held(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReorderStats {
    pub emitted_in_order: usize,
    pub forced_gaps: usize,
    pub forced_capacity: usize,
    pub late_arrivals: usize,
    pub max_hold_observed: usize,
    pub still_held: usize,
}

/// Order-fidelity metric: fraction of event pairs (i<j in true order) that keep
/// their relative order in `observed`. 1.0 = perfect, 0.5 = random.
pub fn order_agreement(true_order: &[String], observed: &[String]) -> f64 {
    let pos: std::collections::HashMap<&str, usize> = observed
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let mut total = 0usize;
    let mut agree = 0usize;
    // O(n^2) is fine for the measured stream sizes; it is exact, not sampled.
    for i in 0..true_order.len() {
        for j in (i + 1)..true_order.len() {
            let (Some(&pi), Some(&pj)) = (
                pos.get(true_order[i].as_str()),
                pos.get(true_order[j].as_str()),
            ) else {
                continue;
            };
            total += 1;
            if pi < pj {
                agree += 1;
            }
        }
    }
    if total == 0 {
        1.0
    } else {
        agree as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{EvidenceStatus, Source};

    fn ev(id: &str, seq: u64, ts: u64) -> Event {
        Event::new(
            id,
            Source::ShellBash,
            Some("s".into()),
            seq,
            ts,
            "CommandFinished",
            serde_json::json!({}),
            EvidenceStatus::Declared,
        )
    }

    fn pol() -> ReorderPolicy {
        ReorderPolicy {
            max_hold: 8,
            grace_ns: 1_000,
            initial_sequence: Some(0),
        }
    }

    #[test]
    fn adjacent_swap_is_repaired() {
        let mut r = SeqReorderer::new(pol());
        assert_eq!(r.push(ev("a", 0, 0), 0).len(), 1);
        assert!(r.push(ev("c", 2, 20), 1).is_empty());
        let out = r.push(ev("b", 1, 10), 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.event_id, "b");
        assert_eq!(out[1].event.event_id, "c");
        assert!(!out[0].kind.is_forced());
    }

    #[test]
    fn permanent_gap_flushes_on_clock_not_on_arrival() {
        let mut r = SeqReorderer::new(pol());
        r.push(ev("a", 0, 0), 0);
        assert!(r.push(ev("c", 2, 5_000), 10).is_empty());
        assert_eq!(r.still_held(), 1);
        let out = r.advance(10_000);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].kind, Emit::ForcedGap { .. }));
    }

    #[test]
    fn capacity_bound_flushes_too() {
        let mut r = SeqReorderer::new(ReorderPolicy {
            max_hold: 2,
            grace_ns: u64::MAX,
            initial_sequence: Some(0),
        });
        r.push(ev("a", 0, 0), 0);
        assert!(r.push(ev("c", 2, 0), 1).is_empty());
        assert!(r.push(ev("d", 3, 0), 2).is_empty());
        let out = r.push(ev("e", 4, 0), 3);
        assert!(!out.is_empty());
        assert!(matches!(out[0].kind, Emit::ForcedCapacity { .. }));
    }

    #[test]
    fn agreement_metric_is_sane() {
        let t = vec!["a".to_string(), "b".into(), "c".into()];
        assert!((order_agreement(&t, &t) - 1.0).abs() < 1e-9);
        let rev = vec!["c".to_string(), "b".into(), "a".into()];
        assert!(order_agreement(&t, &rev) < 0.01);
    }

    #[test]
    fn late_arrival_before_watermark_is_flagged_not_stalled() {
        let mut r = SeqReorderer::new(ReorderPolicy {
            max_hold: 64,
            grace_ns: u64::MAX,
            initial_sequence: None,
        });
        // Consumer attaches at sequence 5: everything older must be emitted.
        assert_eq!(r.push(ev("e5", 5, 50), 0).len(), 1);
        let out = r.push(ev("e3", 3, 30), 1);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].kind,
            Emit::LateArrival {
                watermark: 6,
                got: 3
            }
        ));
        assert_eq!(r.still_held(), 0);
        assert_eq!(r.stats().late_arrivals, 1);
    }
}
