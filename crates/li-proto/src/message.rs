//! Frames other than the event envelope: `Ping`, `Ack`, `Pong`, `Error`.
//!
//! The client-to-server discriminator is **presence of `event_id`**, not a
//! `type` tag (`docs/ipc-protocol-v0.md` §2). That is a Phase 0 fix: a closed
//! serde tag on `type` rejected every real event with `unknown_frame_type`, so
//! the test for "this is an event" had to become "it carries an `event_id`".

use serde::{Deserialize, Serialize};

use crate::ProtocolError;
use crate::event::{Event, EventId, SessionId};
use crate::version::{SchemaVersion, negotiate};

// ---------------------------------------------------------------------------
// Client -> server
// ---------------------------------------------------------------------------

/// Flush barrier and liveness probe.
///
/// A `Ping` travels through the **same bounded ingest queue** as events, so its
/// `Pong` is a genuine barrier: when it returns, everything the client sent
/// earlier on that connection has been ingested. This is the only delivery
/// proof the protocol has (C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ping {
    pub schema_version: SchemaVersion,
    /// Echoed back verbatim in the `Pong` so a client can match its own barrier
    /// without trusting arrival order. Not a security token.
    pub nonce: u64,
}

/// The literal `"Ping"` token. A `Ping` is identified on the wire by
/// `"type":"Ping"` **plus the absence of `event_id`** (`docs/ipc-protocol-v0.md`
/// §2) — absence alone is not enough, because two distinct frames would then be
/// indistinguishable.
pub const PING_TYPE: &str = "Ping";

impl Serialize for Ping {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Ping", 3)?;
        st.serialize_field("schema_version", &self.schema_version)?;
        st.serialize_field("type", PING_TYPE)?;
        st.serialize_field("nonce", &self.nonce)?;
        st.end()
    }
}

/// Accepts `"type":"Ping"` and refuses any other `type`, so a mis-typed frame
/// cannot be silently read as a barrier.
impl<'de> Deserialize<'de> for Ping {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Mirror {
            schema_version: SchemaVersion,
            #[serde(rename = "type")]
            frame_type: String,
            nonce: u64,
        }
        use serde::de::Error as _;
        let m = Mirror::deserialize(d)?;
        if m.frame_type != PING_TYPE {
            return Err(D::Error::custom(format!(
                "expected type `{PING_TYPE}`, got `{}`",
                m.frame_type
            )));
        }
        Ok(Ping {
            schema_version: m.schema_version,
            nonce: m.nonce,
        })
    }
}

/// Anything a client may send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    Event(Box<Event>),
    Ping(Ping),
}

impl ClientMessage {
    pub fn kind_str(&self) -> &'static str {
        match self {
            ClientMessage::Event(_) => "Event",
            ClientMessage::Ping(_) => "Ping",
        }
    }
}

/// Flattens to the frame itself.
///
/// There is deliberately **no** `{"kind":"Event","event":{...}}` wrapper: the
/// wire discriminator is "the frame has an `event_id`", so adding a wrapper
/// would change the discriminator and break every Phase 0 client. The encoding
/// of a `ClientMessage` is byte-identical to the encoding of its inner frame,
/// which the round-trip tests assert.
impl Serialize for ClientMessage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ClientMessage::Event(e) => e.serialize(s),
            ClientMessage::Ping(p) => p.serialize(s),
        }
    }
}

// ---------------------------------------------------------------------------
// Server -> client
// ---------------------------------------------------------------------------

/// Best-effort per-event acknowledgement.
///
/// **This is not a delivery guarantee.** 0-C measured 2 684 of 22 390 acks
/// dropped (12.0 %) under a 50-client burst, because the per-connection reply
/// queue is bounded and an unread ack must never stall ingest. Use
/// [`Pong::watermarks`] / [`Pong::counts`] to prove delivery. C2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub schema_version: SchemaVersion,
    pub event_id: EventId,
    pub sequence: u64,
}

/// Highest contiguous sequence seen for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    pub session_id: SessionId,
    /// `None` means "no event from this session has been ingested yet", which
    /// is *not* the same as "ingested up to 0" — the `LateArrival` trap 0-H §4.2
    /// warns about.
    pub contiguous_through: Option<u64>,
    /// Highest `sequence` seen at all, contiguous or not.
    pub highest_seen: Option<u64>,
}

/// Daemon-lifetime counters. Diagnostics only, never a durability claim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub ingested: u64,
    pub dedup_dropped: u64,
    /// Frames whose `sequence` skipped ahead. A diagnostic, **not** an error:
    /// the client advances `sequence` on drop, so a gap means "the client chose
    /// not to send this" (0-C §8).
    pub seq_violations: u64,
    pub rejected: u64,
}

/// Authoritative delivery state at a barrier.
///
/// The watermarks and counts describe only the **daemon's current lifetime**:
/// in-memory counters do not survive a restart (0-C §12), so after a `kill -9`
/// these numbers start from zero while the journal keeps everything. The
/// journal, not this frame, is the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub schema_version: SchemaVersion,
    /// Echo of the `Ping` nonce.
    pub nonce: u64,
    /// Highest contiguous `sequence` ingested per session.
    pub watermarks: Vec<Watermark>,
    pub counts: Counts,
    /// The version the server speaks, so a client can detect a mismatch
    /// **before** sending events. `docs/ipc-protocol-v0.md` §5 lists this as
    /// planned-but-unimplemented in 0-C (`TBD`); Phase 1 puts it on the wire so
    /// the "no negotiation" limitation is at least discoverable.
    pub server_schema_version: SchemaVersion,
}

/// The `code` vocabulary of an [`ErrorFrame`] (`docs/ipc-protocol-v0.md` §4).
///
/// `#[non_exhaustive]`-style behaviour is provided by `#[serde(other)]`: a
/// client must treat an unrecognised code as "this frame was not stored" and
/// must not retry it in a loop, and a future server adding a code must not
/// require a client release to survive it. [`ErrorCode::means_not_stored`]
/// encodes that rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    PayloadTooLarge,
    FrameTooLarge,
    InvalidJson,
    InvalidEnvelope,
    MissingType,
    UnknownFrameType,
    SchemaVersionUnsupported,
    PeerUidRejected,
    /// Any code this build does not know.
    #[serde(other)]
    Other,
}

impl ErrorCode {
    /// Whether the connection survives. `frame_too_large` and
    /// `peer_uid_rejected` are terminal; everything else leaves the connection
    /// usable.
    pub fn closes_connection(&self) -> bool {
        matches!(self, ErrorCode::FrameTooLarge | ErrorCode::PeerUidRejected)
    }

    /// Every error code means "not stored", including `Other`.
    pub fn means_not_stored(&self) -> bool {
        true
    }
}

/// Frame-level rejection. The connection stays usable unless
/// [`ErrorCode::closes_connection`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorFrame {
    pub schema_version: SchemaVersion,
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorFrame {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            code,
            message: message.into(),
        }
    }

    /// Builds the frame for a [`ProtocolError`], preserving the specific cause
    /// in the free-text field while the code stays a fixed token.
    pub fn from_error(err: &ProtocolError) -> Self {
        Self::new(err.code(), err.to_string())
    }
}

/// Anything a server may send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    Ack(Ack),
    Pong(Box<Pong>),
    Error(ErrorFrame),
}

impl ServerMessage {
    pub fn schema_version(&self) -> SchemaVersion {
        match self {
            ServerMessage::Ack(a) => a.schema_version,
            ServerMessage::Pong(p) => p.schema_version,
            ServerMessage::Error(e) => e.schema_version,
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self {
            ServerMessage::Ack(_) => "Ack",
            ServerMessage::Pong(_) => "Pong",
            ServerMessage::Error(_) => "Error",
        }
    }
}

/// Flattens to the frame itself, for the same reason as [`ClientMessage`].
/// The three server frames stay distinguishable by their own fields
/// (`event_id` / `code` / `nonce|watermarks`), so no discriminator is added.
impl Serialize for ServerMessage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ServerMessage::Ack(a) => a.serialize(s),
            ServerMessage::Pong(p) => p.serialize(s),
            ServerMessage::Error(e) => e.serialize(s),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire decoding
// ---------------------------------------------------------------------------

/// A frame that parsed as JSON but whose fields do not fit the shape we
/// expected. Distinct from `invalid_json` on purpose: the peer sent valid JSON
/// and deserves to be told which field is wrong.
fn envelope_err(e: serde_json::Error) -> ProtocolError {
    ProtocolError::InvalidEnvelope {
        field: "envelope",
        reason: e.to_string(),
    }
}

/// Raw client frame, before the discriminator is applied.
///
/// Every field is optional here on purpose: the discriminator itself is "has
/// `event_id`", so it has to be observed *before* a strict envelope parse
/// decides what the frame is. A strict parse first would answer a `Ping` with
/// `invalid_envelope` instead of a `Pong`.
#[derive(Deserialize)]
struct ClientFrameProbe {
    #[serde(default)]
    schema_version: Option<serde_json::Value>,
    #[serde(default)]
    event_id: Option<serde_json::Value>,
    #[serde(default, rename = "type")]
    frame_type: Option<serde_json::Value>,
    #[serde(default)]
    nonce: Option<serde_json::Value>,
}

/// Decodes one client frame.
///
/// Order of checks matters and is deliberate:
/// 1. a JSON object carrying a numeric `schema_version` — the version is
///    validated **first**, so a futuristic frame is reported as a version
///    problem rather than as missing a field it "should" have had;
/// 2. `event_id` present → event;
/// 3. else `"type":"Ping"` and no `event_id` → ping;
/// 4. else `type` present → `unknown_frame_type`;
/// 5. else → `missing_type`.
pub fn decode_client_message(raw: &str) -> Result<ClientMessage, ProtocolError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ProtocolError::InvalidJson(e.to_string()))?;
    if !value.is_object() {
        return Err(ProtocolError::InvalidJson(
            "top-level JSON value is not an object".to_string(),
        ));
    }

    let probe: ClientFrameProbe = serde_json::from_value(value.clone())
        .map_err(|e| ProtocolError::InvalidJson(e.to_string()))?;

    let claimed = probe
        .schema_version
        .as_ref()
        .and_then(serde_json::Value::as_u64)
        .ok_or(ProtocolError::InvalidEnvelope {
            field: "schema_version",
            reason: "missing or not an unsigned integer".to_string(),
        })?;
    let claimed = SchemaVersion(claimed);
    negotiate(claimed)?;

    if probe.event_id.is_some() {
        // Decode into the raw mirror and validate once, so a bad `event_id`
        // reports as `invalid_envelope{field:"event_id"}` and not as
        // `invalid_json` — the JSON was fine.
        let wire: crate::event::EventWire = serde_json::from_value(value).map_err(envelope_err)?;
        let event = Event::from_wire(wire)?;
        return Ok(ClientMessage::Event(Box::new(event)));
    }

    match probe
        .frame_type
        .as_ref()
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .as_deref()
    {
        Some("Ping") => {
            let nonce = probe
                .nonce
                .as_ref()
                .and_then(serde_json::Value::as_u64)
                .ok_or(ProtocolError::InvalidEnvelope {
                    field: "nonce",
                    reason: "missing or not an unsigned integer".to_string(),
                })?;
            Ok(ClientMessage::Ping(Ping {
                schema_version: claimed,
                nonce,
            }))
        }
        Some(_) => Err(ProtocolError::UnknownFrameType),
        None => Err(ProtocolError::MissingType),
    }
}

/// Raw server frame, discriminated by which fields are present.
#[derive(Deserialize)]
struct ServerFrameProbe {
    #[serde(default)]
    schema_version: Option<serde_json::Value>,
    #[serde(default)]
    event_id: Option<serde_json::Value>,
    #[serde(default)]
    code: Option<serde_json::Value>,
    #[serde(default)]
    nonce: Option<serde_json::Value>,
    #[serde(default)]
    watermarks: Option<serde_json::Value>,
}

/// Decodes one server frame: `event_id` → `Ack`, `code` → `Error`, `nonce` or
/// `watermarks` → `Pong`.
///
/// An unrecognised code is preserved as [`ErrorCode::Other`] instead of failing
/// the parse, so a client built against v1 can still read a v1.x server's new
/// rejection and correctly conclude "not stored".
pub fn decode_server_message(raw: &str) -> Result<ServerMessage, ProtocolError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ProtocolError::InvalidJson(e.to_string()))?;
    if !value.is_object() {
        return Err(ProtocolError::InvalidJson(
            "top-level JSON value is not an object".to_string(),
        ));
    }
    let probe: ServerFrameProbe = serde_json::from_value(value.clone())
        .map_err(|e| ProtocolError::InvalidJson(e.to_string()))?;

    let claimed = probe
        .schema_version
        .as_ref()
        .and_then(serde_json::Value::as_u64)
        .ok_or(ProtocolError::InvalidEnvelope {
            field: "schema_version",
            reason: "missing or not an unsigned integer".to_string(),
        })?;
    negotiate(SchemaVersion(claimed))?;

    if probe.event_id.is_some() {
        let ack: Ack = serde_json::from_value(value).map_err(envelope_err)?;
        return Ok(ServerMessage::Ack(ack));
    }
    if probe.code.is_some() {
        let e: ErrorFrame = serde_json::from_value(value).map_err(envelope_err)?;
        return Ok(ServerMessage::Error(e));
    }
    if probe.nonce.is_some() || probe.watermarks.is_some() {
        let p: Pong = serde_json::from_value(value).map_err(envelope_err)?;
        return Ok(ServerMessage::Pong(Box::new(p)));
    }
    Err(ProtocolError::MissingType)
}
