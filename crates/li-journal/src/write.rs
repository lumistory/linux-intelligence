//! The rows the writer accepts. Owned, `Send`, and ordered: the queue is FIFO and
//! there is exactly one thread applying it, so "session before its events" is a
//! guarantee the caller can rely on rather than a race to retry.

use crate::redact::Redaction;

#[derive(Debug, Clone)]
pub struct Session {
    pub session_id: String,
    pub parent_session: Option<String>,
    pub adapter: String,
    pub adapter_version: Option<String>,
    pub shell_pid: Option<i64>,
    pub started_wall: String,
    pub started_mono_ns: i64,
    pub cwd: Option<String>,
    pub host: Option<String>,
    pub boot_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Process {
    pub pid: i64,
    pub starttime: i64,
    pub incarnation_seq: i64,
    pub comm: Option<String>,
    pub ppid: Option<i64>,
    pub session_id: Option<String>,
    pub first_seen_mono_ns: i64,
    /// Sticky: once a `(pid, starttime)` key is seen carrying two descriptions,
    /// no later observation may clear it (0-H §8.3.2). Enforced by
    /// `ambiguous = max(ambiguous, excluded.ambiguous)` in the upsert.
    pub ambiguous: bool,
}

/// One event, already redacted and already marshalled.
///
/// Redaction has happened before this struct exists — see `Journal::event` — so
/// there is no way to hand the writer an unredacted payload.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub event_id: String,
    pub schema_version: i64,
    pub source: String,
    pub session_id: Option<String>,
    pub process_row_id: Option<i64>,
    pub sequence: i64,
    pub ts_mono_ns: i64,
    pub ts_wall: String,
    pub type_: String,
    pub type_class: String,
    pub payload: String,
    pub evidence_status: String,
    pub dedup_group: Option<String>,
    pub duplicate_of: Option<String>,
    pub redaction_rules: Option<String>,
    pub redaction_hits: Option<i64>,
    pub redaction_digest: Option<String>,
    pub fp_type_class: Option<String>,
    pub fp_subject: Option<String>,
    pub fp_bucket_ns: Option<i64>,
    pub ingested_mono_ns: i64,
}

impl EventRow {
    pub fn apply_redaction(&mut self, r: &Redaction) {
        self.redaction_rules = r.rules_csv();
        self.redaction_hits = Some(r.hits as i64);
        self.redaction_digest = r.secret_digest.clone();
    }
}

#[derive(Debug, Clone)]
pub struct Operation {
    pub operation_id: String,
    pub session_id: Option<String>,
    pub label: String,
    pub intent: Option<String>,
    pub state: String,
    pub started_mono_ns: i64,
}

/// An event's membership in an operation, with the credibility of that edge.
#[derive(Debug, Clone)]
pub struct OperationEdge {
    pub event_id: String,
    pub operation_id: String,
    pub attached_by: String,
    pub level: String,
    pub attached_mono_ns: i64,
}

#[derive(Debug, Clone)]
pub struct Relation {
    pub from_event: String,
    pub to_event: String,
    pub kind: String,
    pub level: String,
    pub rule: String,
    pub inferred_at_mono_ns: i64,
}

#[derive(Debug, Clone)]
pub struct Evidence {
    pub relation_id: i64,
    pub source: String,
    pub source_class: String,
    pub evidence_status: String,
    pub independent: bool,
    pub confidence: Option<f64>,
    pub detail: Option<String>,
}

/// Everything the writer will accept. Nothing else in this crate opens the
/// database for writing.
#[derive(Debug, Clone)]
pub enum Write {
    Session(Session),
    Process(Process),
    Event(EventRow),
    Operation(Operation),
    OperationEdge(OperationEdge),
    Relation(Relation),
    Evidence(Evidence),
}

impl Write {
    /// Short name for counters and error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Write::Session(_) => "session",
            Write::Process(_) => "process",
            Write::Event(_) => "event",
            Write::Operation(_) => "operation",
            Write::OperationEdge(_) => "operation_edge",
            Write::Relation(_) => "relation",
            Write::Evidence(_) => "evidence",
        }
    }
}

/// What one flush learned. Delivery proof is this, not a per-event ack (C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Watermark {
    /// Highest `ingest_seq` committed. A consumer that has seen this number has
    /// seen every event with a lower one.
    pub ingest_seq: u64,
    /// Rows committed since the writer started, by kind-agnostic count.
    pub written: u64,
    /// Rows the database refused (a constraint, an FK). Counted, never silent.
    pub failed: u64,
    /// Events dropped by the bounded queue before the writer ever saw them (C1).
    pub dropped: u64,
    /// `event_id` collisions that were ignored on purpose: a resend is not an
    /// error (C3, 0-C T7).
    pub duplicate: u64,
}
