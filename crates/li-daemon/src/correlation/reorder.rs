//! Two-level deduplication (P4-01 / P4-02) and bounded late-arrival reorderer (P4-03 / P4-04).

use std::collections::{BTreeMap, HashMap, HashSet};

use li_journal::read::EventRecord;
use li_journal::type_class;
use li_proto::CROSS_SOURCE_UNCERTAINTY_NS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupDecision {
    Fresh,
    DuplicateEventId,
    /// Same fact under a new `event_id`: never drops the row, only links corroboration.
    CorroborationOf,
}

#[derive(Debug, Clone, Copy)]
pub struct FingerprintPolicy {
    pub window_ns: u64,
    pub require_same_session: bool,
    pub require_same_process: bool,
}

impl Default for FingerprintPolicy {
    fn default() -> Self {
        Self {
            window_ns: 500_000_000,
            require_same_session: true,
            require_same_process: true,
        }
    }
}

impl FingerprintPolicy {
    pub fn keys(&self, ev: &EventRecord) -> Vec<String> {
        let width = self.window_ns.max(1) as i64;
        let base = ev.ts_mono_ns / width;
        let session = if self.require_same_session {
            ev.session_id.as_deref().unwrap_or("-")
        } else {
            "-"
        };
        let subject = ev
            .payload
            .get("command")
            .and_then(|v| v.as_str())
            .or_else(|| ev.payload.get("path").and_then(|v| v.as_str()))
            .or_else(|| ev.payload.get("program").and_then(|v| v.as_str()))
            .unwrap_or(&ev.event_type);
        let proc_tag = if self.require_same_process {
            let pid = ev.payload.get("pid").and_then(|v| v.as_i64());
            let st = ev.payload.get("starttime").and_then(|v| v.as_u64());
            match (pid, st) {
                (Some(p), Some(s)) => format!("{p}:{s}"),
                (Some(p), None) => format!("{p}:-"),
                _ => "-".to_string(),
            }
        } else {
            "-".to_string()
        };
        let tc = type_class(&ev.event_type);
        [base - 1, base, base + 1]
            .into_iter()
            .map(|b| format!("{tc}|{session}|{subject}@{proc_tag}|{b}"))
            .collect()
    }
}

/// Level-1 exact `event_id` + Level-2 semantic fingerprint corroboration (never drops rows).
pub struct TwoLevelDedup {
    policy: FingerprintPolicy,
    seen_ids: HashSet<String>,
    /// fingerprint key -> canonical event_id
    facts: HashMap<String, String>,
}

impl TwoLevelDedup {
    pub fn new(policy: FingerprintPolicy) -> Self {
        Self {
            policy,
            seen_ids: HashSet::new(),
            facts: HashMap::new(),
        }
    }

    pub fn offer(&mut self, ev: &EventRecord) -> (DedupDecision, Option<String>) {
        if !self.seen_ids.insert(ev.event_id.clone()) {
            return (DedupDecision::DuplicateEventId, None);
        }
        let keys = self.policy.keys(ev);
        for k in &keys {
            if let Some(canon) = self.facts.get(k)
                && canon != &ev.event_id
            {
                return (DedupDecision::CorroborationOf, Some(canon.clone()));
            }
        }
        for k in keys {
            self.facts.insert(k, ev.event_id.clone());
        }
        (DedupDecision::Fresh, None)
    }
}

#[derive(Debug, Clone)]
pub enum ReorderOutput {
    InOrder(EventRecord),
    /// Arrived after the grace watermark moved past `ts_mono_ns`; preserved as first-class fact.
    LateArrival(EventRecord),
}

/// Clock-based reorderer with configurable grace window (`grace_ns`, P4-03 / P4-04).
pub struct Reorderer {
    grace_ns: u64,
    watermark_ns: u64,
    buffer: BTreeMap<(u64, i64), EventRecord>,
}

impl Reorderer {
    pub fn new(grace_ns: u64) -> Self {
        Self {
            grace_ns,
            watermark_ns: 0,
            buffer: BTreeMap::new(),
        }
    }

    pub fn push(&mut self, ev: EventRecord, now_mono_ns: u64) -> Vec<ReorderOutput> {
        let ev_ts = u64::try_from(ev.ts_mono_ns).unwrap_or(0);
        if ev_ts < self.watermark_ns {
            return vec![ReorderOutput::LateArrival(ev)];
        }
        self.buffer.insert((ev_ts, ev.ingest_seq), ev);
        self.advance(now_mono_ns)
    }

    pub fn advance(&mut self, now_mono_ns: u64) -> Vec<ReorderOutput> {
        let new_wm = now_mono_ns.saturating_sub(self.grace_ns);
        if new_wm > self.watermark_ns {
            self.watermark_ns = new_wm;
        }
        let mut out = Vec::new();
        let ready_keys: Vec<(u64, i64)> = self
            .buffer
            .keys()
            .take_while(|(ts, _)| *ts <= self.watermark_ns)
            .copied()
            .collect();
        for k in ready_keys {
            if let Some(ev) = self.buffer.remove(&k) {
                out.push(ReorderOutput::InOrder(ev));
            }
        }
        out
    }

    pub fn flush_all(&mut self) -> Vec<ReorderOutput> {
        let keys: Vec<(u64, i64)> = self.buffer.keys().copied().collect();
        keys.into_iter()
            .filter_map(|k| self.buffer.remove(&k).map(ReorderOutput::InOrder))
            .collect()
    }

    pub fn is_cross_source_ambiguous(a_mono_ns: i64, b_mono_ns: i64) -> bool {
        let delta = a_mono_ns.abs_diff(b_mono_ns);
        delta < CROSS_SOURCE_UNCERTAINTY_NS
    }
}
