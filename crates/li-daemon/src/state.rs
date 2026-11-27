//! In-memory session watermarks, bounded deduplication, and journal handoff.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use li_journal::write::{AdapterRecord, Session, SessionEnd};
use li_journal::{Journal, clock};
use li_proto::{
    Counts, DropReport, Event, EventId, EventType, EvidenceStatus, Pong, SchemaVersion, SessionId,
    Source, Watermark,
};

const DEDUP_WINDOW_CAP: usize = 8_192;
const KNOWN_SESSIONS_CAP: usize = 4_096;

#[derive(Debug, Default)]
struct SessionWindow {
    highest_seen: Option<u64>,
    contiguous_through: Option<u64>,
    pending_above: BTreeSet<u64>,
    reported_drops: u64,
}

impl SessionWindow {
    fn observe_seq(&mut self, seq: u64) -> bool {
        let mut violated = false;
        if let Some(high) = self.highest_seen {
            if seq > high + 1 {
                violated = true;
            }
            if seq > high {
                self.highest_seen = Some(seq);
            }
        } else {
            if seq > 0 {
                violated = true;
            }
            self.highest_seen = Some(seq);
        }

        let next_expected = self.contiguous_through.map_or(0, |c| c + 1);
        if seq >= next_expected {
            self.pending_above.insert(seq);
            while let Some(&first) = self.pending_above.first() {
                let want = self.contiguous_through.map_or(0, |c| c + 1);
                if first == want {
                    self.pending_above.remove(&first);
                    self.contiguous_through = Some(first);
                } else {
                    break;
                }
            }
            // Keep pending set bounded if a session runs with permanent gaps.
            while self.pending_above.len() > 4096 {
                let Some(&first) = self.pending_above.first() else {
                    break;
                };
                self.pending_above.remove(&first);
            }
        }
        violated
    }

    fn as_watermark(&self, session_id: SessionId) -> Watermark {
        Watermark {
            session_id,
            contiguous_through: self.contiguous_through,
            highest_seen: self.highest_seen,
        }
    }
}

struct DedupCache {
    cap: usize,
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl DedupCache {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            order: VecDeque::with_capacity(cap.max(1)),
            set: HashSet::with_capacity(cap.max(1)),
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }

    fn insert(&mut self, id: &str) {
        if self.set.contains(id) {
            return;
        }
        if self.order.len() >= self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        let owned = id.to_string();
        self.order.push_back(owned.clone());
        self.set.insert(owned);
    }
}

struct KnownSessions {
    cap: usize,
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl KnownSessions {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            order: VecDeque::with_capacity(cap.max(1)),
            set: HashSet::with_capacity(cap.max(1)),
        }
    }

    fn contains(&self, sid: &str) -> bool {
        self.set.contains(sid)
    }

    fn insert(&mut self, sid: &str) {
        if self.set.contains(sid) {
            return;
        }
        if self.order.len() >= self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        let owned = sid.to_string();
        self.order.push_back(owned.clone());
        self.set.insert(owned);
    }
}

struct TrackerInner {
    sessions: BTreeMap<String, SessionWindow>,
    known_sessions: KnownSessions,
    dedup: DedupCache,
    counts: Counts,
}

impl TrackerInner {
    fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            known_sessions: KnownSessions::new(KNOWN_SESSIONS_CAP),
            dedup: DedupCache::new(DEDUP_WINDOW_CAP),
            counts: Counts::default(),
        }
    }
}

/// Shared daemon state consumed by every UDS connection task and future Phase 2
/// observers.
pub struct DaemonState {
    pub journal: Arc<Journal>,
    pub boot_id: String,
    tracker: Mutex<TrackerInner>,
    drop_seq_counter: Mutex<HashMap<String, u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Stored,
    Duplicate,
    Backpressure,
    Failed(String),
}

impl DaemonState {
    pub fn new(journal: Arc<Journal>, boot_id: String) -> Self {
        Self {
            journal,
            boot_id,
            tracker: Mutex::new(TrackerInner::new()),
            drop_seq_counter: Mutex::new(HashMap::new()),
        }
    }

    /// Seeds known session IDs from the persisted journal on startup so existing
    /// sessions do not trigger duplicate session inserts.
    pub fn seed_known_sessions(&self, sids: impl IntoIterator<Item = String>) {
        if let Ok(mut guard) = self.tracker.lock() {
            for sid in sids {
                guard.known_sessions.insert(&sid);
            }
        }
    }

    pub fn note_rejected(&self) {
        if let Ok(mut guard) = self.tracker.lock() {
            guard.counts.rejected += 1;
        }
    }

    /// Ensures the `sessions` row exists before any `events` row referencing
    /// `session_id` hits SQLite (`PRAGMA foreign_keys=ON`).
    fn ensure_session_locked(&self, guard: &mut TrackerInner, sid: &str, event: Option<&Event>) {
        if guard.known_sessions.contains(sid) {
            return;
        }
        let session = build_session_row(sid, &self.boot_id, event);
        self.journal.session(session);
        guard.known_sessions.insert(sid);
    }

    /// Normalizes clocks (N11), guarantees FK session row, deduplicates within
    /// window, updates watermarks, and submits to the single SQLite writer.
    pub fn ingest_event(&self, mut event: Event) -> IngestOutcome {
        if event.ts_mono_ns == 0 {
            event.ts_mono_ns = clock::mono_ns().unwrap_or(0);
        }

        let mut guard = match self.tracker.lock() {
            Ok(g) => g,
            Err(_) => return IngestOutcome::Failed("tracker mutex poisoned".to_string()),
        };

        if guard.dedup.contains(event.event_id.as_str()) {
            guard.counts.dedup_dropped += 1;
            return IngestOutcome::Duplicate;
        }

        if let Some(ref sid) = event.session_id {
            self.ensure_session_locked(&mut guard, sid.as_str(), Some(&event));
        }

        match self.journal.event(&event) {
            Ok((li_journal::Accepted::Queued, _)) => {
                guard.dedup.insert(event.event_id.as_str());
                if let Some(ref sid) = event.session_id {
                    let win = guard.sessions.entry(sid.as_str().to_string()).or_default();
                    if win.observe_seq(event.sequence) {
                        guard.counts.seq_violations += 1;
                    }
                }
                if event.event_type.as_str() == "SessionStarted"
                    && let Some(cap_val) = event.payload.get("capability_record")
                    && let Ok(cap_json) = serde_json::to_string(cap_val)
                {
                    let adapter = event
                        .payload
                        .get("shell")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| event.source.as_str())
                        .to_string();
                    let version = event
                        .payload
                        .get("shell_version")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    self.journal.adapter(AdapterRecord {
                        adapter,
                        version,
                        capabilities: cap_json,
                        verified_wall: Some(event.ts_wall.clone()),
                        notes: Some("published at SessionStarted".into()),
                    });
                }
                if event.event_type.as_str() == "SessionEnded"
                    && let Some(ref sid) = event.session_id
                {
                    self.journal.session_end(SessionEnd {
                        session_id: sid.as_str().to_string(),
                        ended_wall: event.ts_wall.clone(),
                        ended_mono_ns: i64::try_from(event.ts_mono_ns).unwrap_or(i64::MAX),
                    });
                }
                guard.counts.ingested += 1;
                IngestOutcome::Stored
            }
            Ok((li_journal::Accepted::Dropped, _)) => IngestOutcome::Backpressure,
            Err(e) => {
                guard.counts.rejected += 1;
                IngestOutcome::Failed(e.to_string())
            }
        }
    }

    /// Records a client-side `DropReport` as a `ProducerDropReported` journal event
    /// so restarts and audits can distinguish intentional drops from transport loss.
    pub fn ingest_drop_report(&self, report: DropReport) -> Result<(), String> {
        let mut guard = self
            .tracker
            .lock()
            .map_err(|_| "tracker mutex poisoned".to_string())?;
        let sid_str = report.session_id.as_str().to_string();
        self.ensure_session_locked(&mut guard, &sid_str, None);
        let win = guard.sessions.entry(sid_str.clone()).or_default();
        win.reported_drops = win.reported_drops.saturating_add(report.dropped);
        drop(guard);

        let n = {
            let mut ctr = self
                .drop_seq_counter
                .lock()
                .map_err(|_| "drop counter poisoned".to_string())?;
            let entry = ctr.entry(sid_str.clone()).or_insert(0);
            *entry += 1;
            *entry
        };

        let mono = clock::mono_ns().unwrap_or(0);
        let wall = clock::wall_now_rfc3339_ms();
        let mut payload = serde_json::Map::new();
        payload.insert("session_id".into(), serde_json::json!(sid_str));
        payload.insert("dropped".into(), serde_json::json!(report.dropped));
        payload.insert("reason".into(), serde_json::json!(report.reason));
        if let Some(last) = report.last_sequence {
            payload.insert("last_sequence".into(), serde_json::json!(last));
        }
        let eid_raw = format!(
            "drop-{}-{n}-{mono}",
            sid_str.chars().take(64).collect::<String>()
        );
        let event = Event::new(
            EventId::new(eid_raw).map_err(|e| e.to_string())?,
            Source::Policy,
            None,
            0,
            mono,
            wall,
            EventType::new("ProducerDropReported").map_err(|e| e.to_string())?,
            payload,
            EvidenceStatus::Declared,
        )
        .map_err(|e| e.to_string())?;

        self.journal
            .event(&event)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn build_pong(&self, nonce: u64) -> Pong {
        let (watermarks, counts) = match self.tracker.lock() {
            Ok(guard) => {
                let wms = guard
                    .sessions
                    .iter()
                    .filter_map(|(sid, win)| {
                        SessionId::new(sid.clone())
                            .ok()
                            .map(|id| win.as_watermark(id))
                    })
                    .collect();
                (wms, guard.counts)
            }
            Err(_) => (Vec::new(), Counts::default()),
        };
        Pong {
            schema_version: SchemaVersion::CURRENT,
            nonce,
            watermarks,
            counts,
            server_schema_version: SchemaVersion::CURRENT,
        }
    }
}

fn build_session_row(sid: &str, boot_id: &str, event: Option<&Event>) -> Session {
    let mono = event
        .map(|e| e.ts_mono_ns)
        .filter(|&m| m > 0)
        .or_else(clock::mono_ns)
        .unwrap_or(0);
    let wall = event
        .map(|e| e.ts_wall.clone())
        .unwrap_or_else(clock::wall_now_rfc3339_ms);
    let adapter = event
        .and_then(|e| e.payload.get("shell").and_then(|v| v.as_str()))
        .or_else(|| event.map(|e| e.source.as_str()))
        .unwrap_or("unknown")
        .to_string();
    let adapter_version = event
        .and_then(|e| e.payload.get("shell_version").and_then(|v| v.as_str()))
        .map(str::to_string);
    let shell_pid = event.and_then(|e| e.payload.get("pid").and_then(|v| v.as_i64()));
    let cwd = event
        .and_then(|e| e.payload.get("cwd").and_then(|v| v.as_str()))
        .map(str::to_string);

    Session {
        session_id: sid.to_string(),
        parent_session: None,
        adapter,
        adapter_version,
        shell_pid,
        started_wall: wall,
        started_mono_ns: i64::try_from(mono).unwrap_or(i64::MAX),
        cwd,
        host: None,
        boot_id: Some(boot_id.to_string()),
    }
}
