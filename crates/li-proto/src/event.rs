//! The event envelope (`docs/ipc-protocol-v0.md` §3) and its closed vocabulary
//! helpers.

use serde::{Deserialize, Serialize};

use crate::check;
use crate::version::{SchemaVersion, negotiate};
use crate::{CROSS_SOURCE_UNCERTAINTY_NS, ProtocolError};

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Globally unique event identity — **the** deduplication key.
///
/// 0-C test 7: 21/21 byte-identical resends deduplicated on `event_id` alone,
/// and a resend of the same `sequence` under a new `event_id` was correctly
/// accepted. C3 therefore makes this the only key a receiver may drop rows on.
///
/// The wire does not care whether it is a UUIDv4, a UUIDv7 or a snowflake: the
/// contract is uniqueness, and parsing the shape would only invite a receiver
/// to reject a valid producer. Only the properties a journal needs are
/// enforced: non-empty, `<= 128` bytes, printable non-space ASCII.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventId(String);

impl EventId {
    pub fn new(s: impl Into<String>) -> Result<Self, ProtocolError> {
        let s = s.into();
        check::id(&s).map_err(|reason| ProtocolError::InvalidEnvelope {
            field: "event_id",
            reason: reason.to_string(),
        })?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EventId {
    type Error = ProtocolError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<EventId> for String {
    fn from(v: EventId) -> String {
        v.0
    }
}

impl core::fmt::Display for EventId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable for one shell / pty / script lifetime.
///
/// **Not** globally unique across reboots (`docs/ipc-protocol-v0.md` §3), so it
/// is only meaningful paired with a boot identity or the session's start
/// timestamp. `None` on an event means "this fact is not attributable to a
/// session" — which is normal for `proc`/`fs` observations, since a inotify
/// event carries no pid and therefore no session (0-E B8/B10).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

impl SessionId {
    pub fn new(s: impl Into<String>) -> Result<Self, ProtocolError> {
        let s = s.into();
        check::id(&s).map_err(|reason| ProtocolError::InvalidEnvelope {
            field: "session_id",
            reason: reason.to_string(),
        })?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionId {
    type Error = ProtocolError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<SessionId> for String {
    fn from(v: SessionId) -> String {
        v.0
    }
}

impl core::fmt::Display for SessionId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

/// Who is claiming the fact. Closed set: these are code paths in *this*
/// program, not user data, so a closed enum cannot break a third party.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Source {
    #[serde(rename = "shell.bash")]
    ShellBash,
    #[serde(rename = "shell.zsh")]
    ShellZsh,
    #[serde(rename = "shell.fish")]
    ShellFish,
    #[serde(rename = "script")]
    Script,
    #[serde(rename = "pty")]
    Pty,
    #[serde(rename = "proc")]
    Proc,
    #[serde(rename = "fs")]
    Fs,
    #[serde(rename = "recovery")]
    Recovery,
    #[serde(rename = "policy")]
    Policy,
}

/// Independence class — the unit at which corroboration is counted.
///
/// 0-H §4.4: two claims corroborate each other only if their source classes
/// differ **and** neither was derived from the other. Phase 1 has exactly three
/// classes, which is why `Strong` (needs two independent classes) is reachable
/// but rare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceClass {
    /// Kernel-observed process facts (procfs).
    KernelProc,
    /// Kernel-observed filesystem facts (inotify).
    KernelFs,
    /// Self-reported by a hook, a script, a provider or the user.
    Declared,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::ShellBash => "shell.bash",
            Source::ShellZsh => "shell.zsh",
            Source::ShellFish => "shell.fish",
            Source::Script => "script",
            Source::Pty => "pty",
            Source::Proc => "proc",
            Source::Fs => "fs",
            Source::Recovery => "recovery",
            Source::Policy => "policy",
        }
    }

    pub fn class(self) -> SourceClass {
        match self {
            Source::Proc => SourceClass::KernelProc,
            Source::Fs => SourceClass::KernelFs,
            Source::ShellBash
            | Source::ShellZsh
            | Source::ShellFish
            | Source::Script
            | Source::Pty
            | Source::Recovery
            | Source::Policy => SourceClass::Declared,
        }
    }
}

// ---------------------------------------------------------------------------
// Evidence status (envelope) vs credibility level (relation)
// ---------------------------------------------------------------------------

/// **Where the claim came from.** Three values, fixed by Phase 0
/// (`docs/00-phase0-protocol.md` §2) and by `docs/ipc-protocol-v0.md` §3.
///
/// This is *not* the five-level credibility scale. See [`CredibilityLevel`] for
/// that and for why the two must not be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceStatus {
    /// Read from the kernel (procfs, inotify, `SO_PEERCRED`) or a first-hand
    /// fact of the emitting process.
    Observed,
    /// Self-reported and possibly wrong: a hook fired twice, a user typed a lie.
    Declared,
    /// Produced by correlation. **Never a raw event's status.**
    Inferred,
}

impl EvidenceStatus {
    /// The wire string. Pinned here so the contract is a value, not a
    /// `Debug` rendering that a refactor can rename.
    pub const fn as_str(self) -> &'static str {
        match self {
            EvidenceStatus::Observed => "observed",
            EvidenceStatus::Declared => "declared",
            EvidenceStatus::Inferred => "inferred",
        }
    }

    pub const ALL: [EvidenceStatus; 3] = [
        EvidenceStatus::Observed,
        EvidenceStatus::Declared,
        EvidenceStatus::Inferred,
    ];
}

impl core::fmt::Display for EvidenceStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The five credibility levels from 0-H §4.4.
///
/// # Why this is not `evidence_status`
///
/// `evidence_status` answers "who said it" and has three values; it is a
/// **property of one event**, fixed on the wire since Phase 0.
/// `CredibilityLevel` answers "how much do we stand on" for a **relation
/// between events**, and 0-H §4.6 is explicit that it lives on the edge, not on
/// the event: one event was measured sitting in two operations at two different
/// levels (`op-edit` Probable/R5, `op-recover` Strong/R4).
///
/// Widening `evidence_status` to five values would therefore be two errors at
/// once: it would break the v0 wire contract that Phase 0 fixed, and it would
/// put a relation-level judgement on an event row, where it can be silently
/// stale. So the five levels live here as their own type, and an
/// [`crate::Event`] never carries one.
///
/// Ordering is deliberate: `Unknown < Weak < Probable < Strong < Confirmed`, so
/// monotonicity (0-H: adding a corroboration must never lower the level) is a
/// `>=` on this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredibilityLevel {
    /// Nothing to stand on, or a broken identity (`R0`/`R1`/`R2`/`R7`).
    Unknown,
    /// Time-window overlap only (`R6`).
    Weak,
    /// Actor plus lineage, but no file evidence tied to the command (`R5`).
    Probable,
    /// Two independent source classes, at least one kernel-observed (`R4`).
    Strong,
    /// Keyed actor, containment known, path tied to the command text (`R3`).
    Confirmed,
}

impl CredibilityLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            CredibilityLevel::Confirmed => "confirmed",
            CredibilityLevel::Strong => "strong",
            CredibilityLevel::Probable => "probable",
            CredibilityLevel::Weak => "weak",
            CredibilityLevel::Unknown => "unknown",
        }
    }

    /// Stable, complete list — the test that pins serde strings iterates this.
    pub const ALL: [CredibilityLevel; 5] = [
        CredibilityLevel::Confirmed,
        CredibilityLevel::Strong,
        CredibilityLevel::Probable,
        CredibilityLevel::Weak,
        CredibilityLevel::Unknown,
    ];
}

impl core::fmt::Display for CredibilityLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Event type: open vocabulary, advisory classification
// ---------------------------------------------------------------------------

/// The `type` field: an **open string**, never a serde tag.
///
/// During 0-C, treating `type` as a closed serde tag rejected *every* event with
/// `unknown_frame_type`; the discriminator for "is this an event" had to be
/// changed to "has `event_id`". `type` therefore stays a string on the wire and
/// an unknown value must be **stored**, not refused.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventType(String);

impl EventType {
    pub fn new(s: impl Into<String>) -> Result<Self, ProtocolError> {
        let s = s.into();
        if s.is_empty() {
            return Err(ProtocolError::InvalidEnvelope {
                field: "type",
                reason: "must not be empty".to_string(),
            });
        }
        if s.len() > 64 || !s.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(ProtocolError::InvalidEnvelope {
                field: "type",
                reason: "must be <= 64 printable non-space ASCII bytes".to_string(),
            });
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Advisory classification. `None` means "a type this build has no opinion
    /// about" — which is legal and must not cause a reject.
    pub fn kind(&self) -> Option<EventKind> {
        EventKind::from_wire(&self.0)
    }
}

impl core::fmt::Display for EventType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The event types Phase 1 knows by name.
///
/// This is a *lookup table*, not the wire vocabulary. `EventType` remains the
/// authority; this enum exists so that code can `match` without string
/// comparison and so that every name has exactly one spelling in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventKind {
    SessionStarted,
    SessionEnded,
    CommandStarted,
    CommandFinished,
    ScriptStarted,
    ScriptFinished,
    ProcessCreated,
    ProcessExited,
    FileCreated,
    FileModified,
    FileDeleted,
    FileRenamed,
    SnapshotCreated,
    RiskDetected,
    ApprovalGranted,
    RecoveryStarted,
    RecoveryCompleted,
    DaemonRestarted,
    LateArrival,
}

impl EventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            EventKind::SessionStarted => "SessionStarted",
            EventKind::SessionEnded => "SessionEnded",
            EventKind::CommandStarted => "CommandStarted",
            EventKind::CommandFinished => "CommandFinished",
            EventKind::ScriptStarted => "ScriptStarted",
            EventKind::ScriptFinished => "ScriptFinished",
            EventKind::ProcessCreated => "ProcessCreated",
            EventKind::ProcessExited => "ProcessExited",
            EventKind::FileCreated => "FileCreated",
            EventKind::FileModified => "FileModified",
            EventKind::FileDeleted => "FileDeleted",
            EventKind::FileRenamed => "FileRenamed",
            EventKind::SnapshotCreated => "SnapshotCreated",
            EventKind::RiskDetected => "RiskDetected",
            EventKind::ApprovalGranted => "ApprovalGranted",
            EventKind::RecoveryStarted => "RecoveryStarted",
            EventKind::RecoveryCompleted => "RecoveryCompleted",
            EventKind::DaemonRestarted => "DaemonRestarted",
            EventKind::LateArrival => "LateArrival",
        }
    }

    pub const ALL: [EventKind; 19] = [
        EventKind::SessionStarted,
        EventKind::SessionEnded,
        EventKind::CommandStarted,
        EventKind::CommandFinished,
        EventKind::ScriptStarted,
        EventKind::ScriptFinished,
        EventKind::ProcessCreated,
        EventKind::ProcessExited,
        EventKind::FileCreated,
        EventKind::FileModified,
        EventKind::FileDeleted,
        EventKind::FileRenamed,
        EventKind::SnapshotCreated,
        EventKind::RiskDetected,
        EventKind::ApprovalGranted,
        EventKind::RecoveryStarted,
        EventKind::RecoveryCompleted,
        EventKind::DaemonRestarted,
        EventKind::LateArrival,
    ];

    /// Canonical spelling first, then a small alias set.
    ///
    /// Two aliases exist because Phase 0 emitted the shorter spellings: the
    /// 0-H prototype's `type_class` matched `SessionStart`/`SessionEnd`, and its
    /// write set used `ProcessStart`/`ProcessExit`. Aliases let those stored
    /// rows keep classifying; they do **not** rewrite the wire string — the
    /// stored `type` is preserved verbatim, because the journal is append-only
    /// and a rewritten type is a rewritten fact.
    ///
    /// `ProcessExec` is deliberately **not** an alias of `ProcessCreated`: exec
    /// is not creation, and merging them would assert a fork edge that was never
    /// observed (0-H §4.4 counts a fork edge as `Confirmed`-worthy evidence).
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "SessionStarted" | "SessionStart" => EventKind::SessionStarted,
            "SessionEnded" | "SessionEnd" => EventKind::SessionEnded,
            "CommandStarted" => EventKind::CommandStarted,
            "CommandFinished" => EventKind::CommandFinished,
            "ScriptStarted" => EventKind::ScriptStarted,
            "ScriptFinished" => EventKind::ScriptFinished,
            "ProcessCreated" | "ProcessStart" => EventKind::ProcessCreated,
            "ProcessExited" | "ProcessExit" => EventKind::ProcessExited,
            "FileCreated" => EventKind::FileCreated,
            "FileModified" => EventKind::FileModified,
            "FileDeleted" => EventKind::FileDeleted,
            "FileRenamed" => EventKind::FileRenamed,
            "SnapshotCreated" => EventKind::SnapshotCreated,
            "RiskDetected" => EventKind::RiskDetected,
            "ApprovalGranted" => EventKind::ApprovalGranted,
            "RecoveryStarted" => EventKind::RecoveryStarted,
            "RecoveryCompleted" => EventKind::RecoveryCompleted,
            "DaemonRestarted" => EventKind::DaemonRestarted,
            "LateArrival" => EventKind::LateArrival,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Optional semantic fingerprint
// ---------------------------------------------------------------------------

/// The coarse "same *fact*, different event" key from 0-H.
///
/// # Contract — read this before using the field
///
/// **This is not a deduplication key and must never replace [`EventId`].**
/// C3 / 0-H §5: exact `event_id` is the authority for *storage* (zero false
/// positives, free); the fingerprint is the authority for *facts*, it recovers
/// what exact-id matching misses, and its false-merge rate is non-zero by
/// construction. A fingerprint match may only **attach a corroboration** to an
/// event that already exists. It may never drop, merge or overwrite a row: a
/// deletion cannot be undone, and 0-H §8.1.1 makes that a gate condition.
///
/// The field is `Option` on the envelope because most producers cannot compute
/// it (a shell hook has no type class) and because a synthetic fingerprint
/// presented as a real one is worse than no fingerprint. Absent means "unknown",
/// not "fresh".
///
/// The parts are carried verbatim instead of a digest: the hash function is a
/// storage concern (0-H's prototype used 64-bit FNV-1a, which its own report
/// calls not collision-resistant enough to trust), and pinning an algorithm
/// into the wire format would make changing it a protocol break.
///
/// Unknown keys are rejected (`deny_unknown_fields`), for the same reason as on
/// the envelope: this is protocol structure, not user data, and a fingerprint
/// whose parts were partly dropped would produce false "same fact" merges —
/// the one operation 0-H §8.1.1 says must never be wrong in the deleting
/// direction. Unknown `payload` keys stay accepted; unknown *protocol* keys do
/// not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticFingerprint {
    /// Coarse class such as `proc.start` / `fs.modify` — not the event type,
    /// because the point is to let two different types describe one fact.
    pub type_class: String,
    /// Part of the key per 0-H §8.1.2: dropping it cost 50 of 50 decidable
    /// negatives in its class. Absent for cross-source matches where the kernel
    /// side has no session (0-H's `require_same_session: false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// Normalised command text or path.
    pub subject: String,
    /// Time bucket index (`ts_mono_ns / window_ns`), so that two reports of one
    /// fact either land in the same bucket or in a neighbour.
    pub bucket_ns: i64,
}

// ---------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------

/// Where two events sit in time relative to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Before,
    After,
    /// Not knowable. Either the same session and the same `sequence`, or two
    /// different source classes closer together than one `CLK_TCK` tick.
    Ambiguous,
}

/// The event envelope from `docs/ipc-protocol-v0.md` §3.
///
/// ## Field-order and encoding commitments
///
/// Serialisation order is the declaration order below and `payload` keys are
/// emitted sorted (BTreeMap), so the encoding of a given event is byte-stable.
/// A gold JSON test in this crate depends on that; without it, "the field name
/// changed silently" is undetectable in review.
///
/// ## Time
///
/// Two timestamps, because one cannot do both jobs:
///
/// * `ts_mono_ns` — `CLOCK_MONOTONIC` nanoseconds, `u64`. Immune to NTP steps
///   and to the user changing the wall clock, which is what an ordering and a
///   duration need. **Not comparable across boots**: a monotonic clock restarts
///   near zero after a reboot, so it must never be used to order events from
///   two different boots. Pair it with the boot id stored by the journal.
/// * `ts_wall` — RFC 3339 UTC with **milliseconds** and a `Z` suffix, as a
///   fixed 24-byte string. This is the one a user searches on, so it must sort
///   lexicographically like a time (it does, at fixed width) and must not be a
///   float or a locale-formatted string. Milliseconds, not nanoseconds: the
///   kernel sources that matter are quantised far coarser (0-E/0-H: one
///   `CLK_TCK` tick = 10 ms), so nanosecond digits in the wall clock would be
///   fabricated precision.
///
/// Neither is a `SystemTime`: that is a syscall-backed type, and this crate has
/// no clock, no syscalls and no I/O. The daemon reads the clock and fills both
/// fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub schema_version: SchemaVersion,
    pub event_id: EventId,
    pub source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub sequence: u64,
    pub ts_mono_ns: u64,
    pub ts_wall: String,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub payload: serde_json::Map<String, serde_json::Value>,
    pub evidence_status: EvidenceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_fingerprint: Option<SemanticFingerprint>,
}

/// Deserialisation mirror.
///
/// `deny_unknown_fields` is the deliberate answer to "what happens when an
/// upstream adds a field": **an unknown top-level envelope field is rejected**
/// (`invalid_envelope`), while unknown `payload` keys are accepted and kept.
///
/// The asymmetry is not arbitrary. `payload` is explicitly open — the protocol
/// says so, and it is type-specific. The *envelope* is a closed, fixed set of
/// nine fields, and a silently ignored envelope field is a silent data bug: a
/// producer that spells `sequence` as `seq` would have its event stored with
/// `sequence: 0`, which makes the receiver's loss detector
/// (`max(sequence)+1-count`) report a gap that does not exist. A loud reject is
/// the only safe reading. Envelope shape may only change with a
/// `schema_version` bump, which is rejected separately and more specifically.
/// Raw envelope as it appears on the wire.
///
/// Every field is kept in its *unvalidated* form on purpose. If the newtypes
/// validated during deserialisation, a bad `event_id` would surface as a serde
/// `custom` error with no field name, and the caller could only report it as
/// `invalid_json` — which is wrong twice over: the JSON parsed, and the
/// operator needs to know *which* field is bad. Validation therefore happens
/// once, in [`Event::from_wire`], where the error can name the field.
pub(crate) struct EventWire {
    pub(crate) schema_version: SchemaVersion,
    pub(crate) event_id: String,
    pub(crate) source: Source,
    pub(crate) session_id: Option<String>,
    pub(crate) sequence: u64,
    pub(crate) ts_mono_ns: u64,
    pub(crate) ts_wall: String,
    pub(crate) event_type: String,
    pub(crate) payload: serde_json::Value,
    pub(crate) evidence_status: EvidenceStatus,
    pub(crate) semantic_fingerprint: Option<SemanticFingerprint>,
}

impl<'de> Deserialize<'de> for EventWire {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Mirror {
            schema_version: SchemaVersion,
            event_id: String,
            source: Source,
            #[serde(default)]
            session_id: Option<String>,
            sequence: u64,
            ts_mono_ns: u64,
            ts_wall: String,
            #[serde(rename = "type")]
            event_type: String,
            payload: serde_json::Value,
            evidence_status: EvidenceStatus,
            #[serde(default)]
            semantic_fingerprint: Option<SemanticFingerprint>,
        }
        let m = Mirror::deserialize(d)?;
        Ok(EventWire {
            schema_version: m.schema_version,
            event_id: m.event_id,
            source: m.source,
            session_id: m.session_id,
            sequence: m.sequence,
            ts_mono_ns: m.ts_mono_ns,
            ts_wall: m.ts_wall,
            event_type: m.event_type,
            payload: m.payload,
            evidence_status: m.evidence_status,
            semantic_fingerprint: m.semantic_fingerprint,
        })
    }
}

/// Deserialises with the same validation the receiver applies.
///
/// A `ProtocolError` is flattened into a serde error here, because `serde` has
/// no way to carry it. Callers that need the precise variant (the frame
/// decoders) use [`EventWire`] plus [`Event::from_wire`] instead.
impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let w = EventWire::deserialize(d)?;
        Event::from_wire(w).map_err(D::Error::custom)
    }
}

impl Event {
    /// Validates a raw envelope. Check order is the contract; see the tests.
    pub(crate) fn from_wire(w: EventWire) -> Result<Self, ProtocolError> {
        // Version first: a future frame must be reported as a *version*
        // problem, not as a missing field because a field moved.
        negotiate(w.schema_version)?;

        let event_id = EventId::try_from(w.event_id).map_err(|e| inner_reason(e, "event_id"))?;
        let session_id = match w.session_id {
            None => None,
            Some(s) => Some(SessionId::try_from(s).map_err(|e| inner_reason(e, "session_id"))?),
        };
        let event_type = EventType::new(w.event_type).map_err(|e| inner_reason(e, "type"))?;

        check::ts_wall(&w.ts_wall).map_err(|reason| ProtocolError::InvalidEnvelope {
            field: "ts_wall",
            reason: reason.to_string(),
        })?;

        let payload = match w.payload {
            serde_json::Value::Object(m) => m,
            other => {
                return Err(ProtocolError::InvalidEnvelope {
                    field: "payload",
                    reason: format!("must be a JSON object, got {}", kind_of(&other)),
                });
            }
        };

        // Invariant: `sequence` counts within a session. An event with no
        // session has nothing to count against, so a non-zero value would be a
        // fabricated gap in a session that does not exist and the receiver's
        // audit would report loss that never happened. Refuse it instead.
        if session_id.is_none() && w.sequence != 0 {
            return Err(ProtocolError::InvalidEnvelope {
                field: "sequence",
                reason: "must be 0 when session_id is absent".to_string(),
            });
        }

        if let Some(fp) = &w.semantic_fingerprint
            && fp.type_class.is_empty()
        {
            return Err(ProtocolError::InvalidEnvelope {
                field: "semantic_fingerprint",
                reason: "type_class must not be empty".to_string(),
            });
        }

        Ok(Event {
            schema_version: w.schema_version,
            event_id,
            source: w.source,
            session_id,
            sequence: w.sequence,
            ts_mono_ns: w.ts_mono_ns,
            ts_wall: w.ts_wall,
            event_type,
            payload,
            evidence_status: w.evidence_status,
            semantic_fingerprint: w.semantic_fingerprint,
        })
    }

    /// Advisory kind, if this build has an opinion about the type.
    pub fn kind(&self) -> Option<EventKind> {
        self.event_type.kind()
    }

    /// Anchor for corroboration counting: the source class, not the source.
    pub fn source_class(&self) -> SourceClass {
        self.source.class()
    }

    /// Ordering, honest about what is not knowable.
    ///
    /// Within one session, `sequence` is the order — it is assigned by the
    /// producer and is exact (0-C §8). Across sessions or source classes the
    /// only common clock is monotonic nanoseconds, quantised by the kernel to a
    /// tick, so 0-H §8.1.4 requiring an explicit ±1 tick uncertainty is
    /// encoded here as [`Order::Ambiguous`] rather than as a guess.
    pub fn order_vs(&self, other: &Event) -> Order {
        if let (Some(a), Some(b)) = (&self.session_id, &other.session_id)
            && a == b
        {
            return match self.sequence.cmp(&other.sequence) {
                core::cmp::Ordering::Less => Order::Before,
                core::cmp::Ordering::Greater => Order::After,
                // Sequences are never reused within a session, so equal means
                // something is wrong — a restart that reset the counter. Say so
                // instead of inventing an order.
                core::cmp::Ordering::Equal => Order::Ambiguous,
            };
        }
        let delta = self.ts_mono_ns.abs_diff(other.ts_mono_ns);
        if delta < CROSS_SOURCE_UNCERTAINTY_NS {
            Order::Ambiguous
        } else if self.ts_mono_ns < other.ts_mono_ns {
            Order::Before
        } else {
            Order::After
        }
    }
}

/// Unwraps a newtype's own `invalid_envelope` into a bare reason, so that the
/// reported field is the one the caller asked about and not `event_id` twice.
fn inner_reason(e: ProtocolError, field: &'static str) -> ProtocolError {
    match e {
        ProtocolError::InvalidEnvelope { reason, .. } => {
            ProtocolError::InvalidEnvelope { field, reason }
        }
        other => ProtocolError::InvalidEnvelope {
            field,
            reason: other.to_string(),
        },
    }
}

fn kind_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

impl Event {
    /// Builds a valid envelope for this build. Test and producer convenience;
    /// it validates exactly what [`Event::from_wire`] validates, so a producer
    /// cannot construct an event the receiver would refuse.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: EventId,
        source: Source,
        session_id: Option<SessionId>,
        sequence: u64,
        ts_mono_ns: u64,
        ts_wall: impl Into<String>,
        event_type: EventType,
        payload: serde_json::Map<String, serde_json::Value>,
        evidence_status: EvidenceStatus,
    ) -> Result<Self, ProtocolError> {
        let ts_wall = ts_wall.into();
        check::ts_wall(&ts_wall).map_err(|reason| ProtocolError::InvalidEnvelope {
            field: "ts_wall",
            reason: reason.to_string(),
        })?;
        if session_id.is_none() && sequence != 0 {
            return Err(ProtocolError::InvalidEnvelope {
                field: "sequence",
                reason: "must be 0 when session_id is absent".to_string(),
            });
        }
        Ok(Event {
            schema_version: SchemaVersion::CURRENT,
            event_id,
            source,
            session_id,
            sequence,
            ts_mono_ns,
            ts_wall,
            event_type,
            payload,
            evidence_status,
            semantic_fingerprint: None,
        })
    }
}
