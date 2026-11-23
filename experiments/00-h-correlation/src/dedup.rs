//! Two-level deduplication.
//!
//! Level 1 — `event_id`: exact, cheap, is the *only* scheme with no false
//! positives, but it cannot see the same fact reported twice under two ids
//! (0-C confirmed this: 21/21 resends hit, because the client resent the same
//! uuid).
//!
//! Level 2 — semantic fingerprint: `(type_class, session, subject, time bucket)`.
//! Deliberately coarse. It answers "these two events are the same *fact*" and
//! nothing else; the physical event is kept, the duplicate is marked as a
//! corroboration of an existing fact.

use crate::envelope::Event;
use crate::util::{fnv_hex, normalize_ws};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    pub type_class: String,
    pub session: String,
    pub subject: String,
    pub bucket: i64,
}

impl Fingerprint {
    pub fn as_key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.type_class, self.session, self.subject, self.bucket
        )
    }

    pub fn digest(&self) -> String {
        fnv_hex(&self.as_key())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMode {
    /// Hash only the bucket containing the timestamp.
    Single,
    /// Hash the timestamp's bucket and its two neighbours; catches straddles at
    /// the cost of a wider confusion interval.
    Triple,
}

#[derive(Debug, Clone, Copy)]
pub struct FingerprintPolicy {
    /// Bucket width in nanoseconds. Must be >= the maximum plausible reporting
    /// skew between the two sources that describe one fact.
    pub window_ns: u64,
    pub mode: WindowMode,
    /// When false, `session_id` is dropped from the key: needed for cross-source
    /// matches where one side has no session (raw procfs/inotify events).
    pub require_same_session: bool,
    /// When true, `(pid, starttime)` is part of the key. This is the difference
    /// between merging "the same command twice" and "the same command twice by
    /// the same process": measured in 0-H §1.3, it is what makes a concurrent
    /// second run of one script distinguishable.
    pub require_same_process: bool,
}

impl Default for FingerprintPolicy {
    fn default() -> Self {
        Self {
            window_ns: 500_000_000,
            mode: WindowMode::Triple,
            require_same_session: false,
            require_same_process: false,
        }
    }
}

impl FingerprintPolicy {
    pub fn buckets(&self, ts_mono_ns: u64) -> Vec<i64> {
        let width = self.window_ns.max(1) as i64;
        let base = (ts_mono_ns as i64) / width;
        match self.mode {
            WindowMode::Single => vec![base],
            WindowMode::Triple => vec![base - 1, base, base + 1],
        }
    }

    pub fn keys(&self, event: &Event) -> Vec<String> {
        let spec = self.parts(event);
        self.buckets(event.ts_mono_ns)
            .into_iter()
            .map(|bucket| {
                Fingerprint {
                    bucket,
                    ..spec.clone()
                }
                .as_key()
            })
            .collect()
    }

    fn parts(&self, event: &Event) -> Fingerprint {
        let session = if self.require_same_session {
            event.session_id.clone().unwrap_or_else(|| "-".to_string())
        } else {
            // Cross-source identity: session is not part of the key, because
            // the procfs poller has no idea which shell spawned the process.
            "-".to_string()
        };
        let subject = event
            .subject()
            .unwrap_or_else(|| normalize_ws(&event.type_))
            .to_string();
        let subject = if self.require_same_process {
            let proc = event
                .pid_key()
                .map(|k| k.as_str())
                .unwrap_or_else(|| "-".to_string());
            format!("{subject}@{proc}")
        } else {
            subject
        };
        Fingerprint {
            type_class: event.type_class().to_string(),
            session,
            subject,
            bucket: 0,
        }
    }
}

/// Result of offering one event to the deduplicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing matched: genuinely new fact.
    Fresh,
    /// Same `event_id` already seen: transport-level duplicate.
    DuplicateEventId,
    /// Different `event_id`, same fact: semantic duplicate, keep as corroboration.
    DuplicateSemantic,
}

#[derive(Debug, Default)]
pub struct Deduplicator {
    policy: FingerprintPolicy,
    seen_ids: std::collections::HashSet<String>,
    /// fingerprint key -> canonical event id
    seen_facts: std::collections::HashMap<String, String>,
}

impl Deduplicator {
    pub fn new(policy: FingerprintPolicy) -> Self {
        Self {
            policy,
            ..Default::default()
        }
    }

    pub fn offer(&mut self, event: &Event) -> Decision {
        if !self.seen_ids.insert(event.event_id.clone()) {
            return Decision::DuplicateEventId;
        }
        for key in self.policy.keys(event) {
            if let Some(canonical) = self.seen_facts.get(&key) {
                if canonical != &event.event_id {
                    return Decision::DuplicateSemantic;
                }
            }
        }
        for key in self.policy.keys(event) {
            self.seen_facts.insert(key, event.event_id.clone());
        }
        Decision::Fresh
    }

    pub fn facts(&self) -> usize {
        self.seen_facts.len()
    }
}

/// Scoring harness output.
#[derive(Debug, Clone, Default)]
pub struct Score {
    /// True pairs identified as duplicates.
    pub true_pos: usize,
    /// Distinct facts merged into one: the dangerous error.
    pub false_pos: usize,
    /// Duplicates not identified.
    pub false_neg: usize,
    pub true_neg: usize,
}

impl Score {
    pub fn precision(&self) -> f64 {
        let d = self.true_pos + self.false_pos;
        if d == 0 {
            1.0
        } else {
            self.true_pos as f64 / d as f64
        }
    }

    pub fn recall(&self) -> f64 {
        let d = self.true_pos + self.false_neg;
        if d == 0 {
            1.0
        } else {
            self.true_pos as f64 / d as f64
        }
    }

    pub fn false_merge_rate(&self) -> f64 {
        let d = self.true_pos + self.false_pos;
        if d == 0 {
            0.0
        } else {
            self.false_pos as f64 / d as f64
        }
    }
}

/// Ground truth the generator attaches to every synthetic event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    /// First appearance of a fact.
    Origin,
    /// Same fact, same id (resend).
    ResendSameId,
    /// Same fact, fresh id, from another source.
    SameFactNewId,
    /// A genuinely different fact that looks similar.
    Distinct,
}

impl Truth {
    pub fn as_str(self) -> &'static str {
        match self {
            Truth::Origin => "origin",
            Truth::ResendSameId => "resend-same-id",
            Truth::SameFactNewId => "same-fact-new-id",
            Truth::Distinct => "distinct",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Labelled {
    pub event: Event,
    pub truth: Truth,
    /// The `event_id` of the origin event this one duplicates, if any.
    pub origin: Option<String>,
}

/// Score a dedup run against ground truth.
pub fn score(labelled: &[Labelled], decisions: &[Decision]) -> Score {
    let mut s = Score::default();
    for (l, d) in labelled.iter().zip(decisions.iter()) {
        let is_dup = matches!(l.truth, Truth::ResendSameId | Truth::SameFactNewId);
        let called_dup = !matches!(d, Decision::Fresh);
        match (is_dup, called_dup) {
            (true, true) => s.true_pos += 1,
            (false, true) => s.false_pos += 1,
            (true, false) => s.false_neg += 1,
            (false, false) => s.true_neg += 1,
        }
    }
    s
}

/// Exact-id-only baseline, for the comparison table.
pub fn score_exact_only(labelled: &[Labelled]) -> Score {
    let mut seen = std::collections::HashSet::new();
    let decisions: Vec<Decision> = labelled
        .iter()
        .map(|l| {
            if seen.insert(l.event.event_id.clone()) {
                Decision::Fresh
            } else {
                Decision::DuplicateEventId
            }
        })
        .collect();
    score(labelled, &decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{EvidenceStatus, Source};

    fn ev(id: &str, source: Source, ts: u64, ty: &str, cmd: &str) -> Event {
        Event::new(
            id,
            source,
            Some("s1".into()),
            1,
            ts,
            ty,
            serde_json::json!({"command": cmd, "pid": 10, "starttime": 20}),
            EvidenceStatus::Declared,
        )
    }

    #[test]
    fn exact_id_is_caught() {
        let mut d = Deduplicator::new(FingerprintPolicy::default());
        let e = ev("a", Source::ShellBash, 1_000, "CommandFinished", "ls");
        assert_eq!(d.offer(&e), Decision::Fresh);
        assert_eq!(d.offer(&e), Decision::DuplicateEventId);
    }

    #[test]
    fn cross_source_same_command_caught_semantically() {
        let mut d = Deduplicator::new(FingerprintPolicy::default());
        let a = ev("a", Source::ShellBash, 1_000_000_000, "CommandFinished", "ls -l");
        let b = ev("b", Source::Proc, 1_020_000_000, "ProcessExit", "ls  -l");
        assert_eq!(d.offer(&a), Decision::Fresh);
        assert_eq!(d.offer(&b), Decision::DuplicateSemantic);
    }

    #[test]
    fn same_command_far_apart_is_not_a_duplicate() {
        let mut d = Deduplicator::new(FingerprintPolicy::default());
        let a = ev("a", Source::ShellBash, 1_000_000_000, "CommandFinished", "ls");
        let b = ev("b", Source::ShellBash, 9_000_000_000, "CommandFinished", "ls");
        assert_eq!(d.offer(&a), Decision::Fresh);
        assert_eq!(d.offer(&b), Decision::Fresh);
    }

    #[test]
    fn different_command_in_same_window_is_not_a_duplicate() {
        let mut d = Deduplicator::new(FingerprintPolicy::default());
        let a = ev("a", Source::ShellBash, 1_000_000_000, "CommandFinished", "ls");
        let b = ev("b", Source::Proc, 1_010_000_000, "ProcessExit", "rm -rf /tmp/x");
        assert_eq!(d.offer(&a), Decision::Fresh);
        assert_eq!(d.offer(&b), Decision::Fresh);
    }
}
