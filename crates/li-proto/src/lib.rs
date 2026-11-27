//! Wire types for the Linux Intelligence IPC protocol, v0.
//!
//! # What this crate is
//!
//! Pure data types, validation and JSON encoding. **No I/O, no async, no
//! clock, no syscalls, no `unsafe`.** Every rule here is copied from
//! `docs/ipc-protocol-v0.md` (line format) or from a Phase 0 measurement named
//! in the doc comment of the item it constrains. Where a Phase 0 constraint is
//! verifiable in the type system, it is encoded here rather than documented:
//!
//! * C3 — `event_id` is the only deduplication key. [`SemanticFingerprint`] is
//!   `Option` and explicitly labelled "attach only".
//! * C1/C2 — `sequence` advances on drop, so it is a plain `u64` starting at
//!   `0`; a gap is *data*, not an error. [`Watermark`] is delivery proof, an
//!   [`Ack`] is not.
//! * Open vocabulary — `type` is never a serde tag (a closed tag rejected every
//!   event with `unknown_frame_type` during 0-C). [`EventType`] is a string
//!   newtype with an *advisory* [`EventKind`] view.
//! * 0-H §4.4 — credibility is five-valued and lives on a *relation*, not on an
//!   event. See [`CredibilityLevel`] and the note on [`EvidenceStatus`].
//!
//! # Encoding
//!
//! One JSON object per line, `\n` terminated, UTF-8, no BOM, `1 MiB` per frame
//! ([`MAX_FRAME_BYTES`]). JSON is chosen over CBOR for v0 because the transport
//! is already text and diffable and the measured headroom (116k–139k events/s
//! in 0-C) makes compactness irrelevant next to auditability.
//!
//! [`Event::to_json_line`] returns a string **without** the trailing newline;
//! the caller appends `\n`. serde escapes `\n` inside strings as `\n`, so a
//! frame can never contain a raw `0x0A` that would split it.

pub mod event;
pub mod json;
pub mod message;
pub mod version;

pub use event::{
    CredibilityLevel, Event, EventId, EventKind, EventType, EvidenceStatus, SemanticFingerprint,
    SessionId, Source,
};
pub use json::{check_inbound_frame_len, encode_frame};
pub use message::{
    Ack, ClientMessage, Counts, DropReport, ErrorCode, ErrorFrame, Ping, Pong, ServerMessage,
    Watermark, decode_client_message, decode_server_message,
};
pub use version::{CURRENT_SCHEMA_VERSION, SchemaVersion, negotiate};

/// Hard per-frame ceiling. Frames above this are rejected with
/// [`ErrorCode::PayloadTooLarge`] and the connection stays usable.
///
/// Measured in 0-C test 3, including a 10 MiB frame being rejected with the
/// connection surviving.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Above this, the receiver gives up on the frame entirely and closes the
/// connection ([`ErrorCode::FrameTooLarge`]). Bounds the buffer a hostile or
/// broken peer can pin, per `docs/ipc-protocol-v0.md` §1.
pub const DISCARD_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Cross-source ordering uncertainty: one `CLK_TCK` tick at `HZ=100`.
///
/// 0-H §4.3 measured 40 forks landing on 2 distinct `starttime` ticks, and
/// 0-H §8.1.4 requires that any stored cross-source order carry this
/// uncertainty explicitly rather than claim a better resolution.
pub const CROSS_SOURCE_UNCERTAINTY_NS: u64 = 10_000_000;

/// Everything that can be wrong with a frame.
///
/// Hand-written `Display`/`Error` instead of a derive crate: `li-hook` links
/// this type on the shell's hot path, so the dependency budget is a design
/// constraint (spec §1), not a preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// Frame is not parseable JSON.
    InvalidJson(String),
    /// JSON object, but an envelope field is missing or ill-typed.
    InvalidEnvelope { field: &'static str, reason: String },
    /// Neither `event_id` nor `type` is present.
    MissingType,
    /// Has `type`, no `event_id`, and is neither `Ping` nor `DropReport`.
    UnknownFrameType,
    /// `schema_version` is not [`CURRENT_SCHEMA_VERSION`]. Rejected per frame,
    /// never silently accepted: a future field set would otherwise be read as
    /// a v1 event with defaults.
    SchemaVersionUnsupported(u64),
    /// Frame exceeded [`MAX_FRAME_BYTES`].
    PayloadTooLarge(usize),
    /// Frame exceeded [`DISCARD_CAP_BYTES`].
    FrameTooLarge(usize),
}

impl ProtocolError {
    /// The wire `code` this error maps to (`docs/ipc-protocol-v0.md` §4).
    pub fn code(&self) -> ErrorCode {
        match self {
            ProtocolError::InvalidJson(_) => ErrorCode::InvalidJson,
            ProtocolError::InvalidEnvelope { .. } => ErrorCode::InvalidEnvelope,
            ProtocolError::MissingType => ErrorCode::MissingType,
            ProtocolError::UnknownFrameType => ErrorCode::UnknownFrameType,
            ProtocolError::SchemaVersionUnsupported(_) => ErrorCode::SchemaVersionUnsupported,
            ProtocolError::PayloadTooLarge(_) | ProtocolError::FrameTooLarge(_) => {
                ErrorCode::PayloadTooLarge
            }
        }
    }
}

impl core::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProtocolError::InvalidJson(m) => write!(f, "invalid_json: {m}"),
            ProtocolError::InvalidEnvelope { field, reason } => {
                write!(f, "invalid_envelope: field `{field}`: {reason}")
            }
            ProtocolError::MissingType => {
                write!(f, "missing_type: neither `event_id` nor `type` present")
            }
            ProtocolError::UnknownFrameType => write!(
                f,
                "unknown_frame_type: has `type`, no `event_id`, and is neither `Ping` nor `DropReport`"
            ),
            ProtocolError::SchemaVersionUnsupported(v) => write!(
                f,
                "schema_version_unsupported: got {v}, this build speaks {}",
                CURRENT_SCHEMA_VERSION
            ),
            ProtocolError::PayloadTooLarge(n) => {
                write!(f, "payload_too_large: {n} bytes > {MAX_FRAME_BYTES}")
            }
            ProtocolError::FrameTooLarge(n) => {
                write!(f, "frame_too_large: {n} bytes > {DISCARD_CAP_BYTES}")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Borrowed helpers shared by the validation points.
pub(crate) mod check {
    /// Opaque identifiers (`event_id`, `session_id`) are `1..=128` bytes of
    /// printable non-space ASCII. Deliberately *not* a UUID parser: uniqueness
    /// is the contract (0-C test 7), and the wire never inspects the shape. An
    /// id that cannot round-trip through a JSON object key or a SQLite text
    /// column is rejected here instead.
    pub(crate) const MAX_ID_BYTES: usize = 128;

    pub(crate) fn id(s: &str) -> Result<(), &'static str> {
        if s.is_empty() {
            return Err("must not be empty");
        }
        if s.len() > MAX_ID_BYTES {
            return Err("longer than 128 bytes");
        }
        if !s.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("must be printable non-space ASCII");
        }
        Ok(())
    }

    /// `ts_wall` is RFC 3339 UTC with **milliseconds** and a `Z` suffix, e.g.
    /// `2026-11-23T18:00:00.000Z`. Fixed width so that a lexicographic sort is
    /// a time sort, which is what the journal relies on for range queries.
    pub(crate) fn ts_wall(s: &str) -> Result<(), &'static str> {
        const SHAPE: &[u8] = b"dddd-dd-ddTdd:dd:dd.dddZ";
        let b = s.as_bytes();
        if b.len() != SHAPE.len() {
            return Err("must be `YYYY-MM-DDTHH:MM:SS.mmmZ` (24 bytes)");
        }
        for (i, want) in SHAPE.iter().enumerate() {
            match want {
                b'd' if !b[i].is_ascii_digit() => return Err("non-digit in timestamp"),
                b'-' | b':' | b'T' | b'.' | b'Z' if b[i] != *want => {
                    return Err("misplaced separator in timestamp");
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_check_rejects_empty_control_and_oversize() {
        assert!(check::id("").is_err());
        assert!(check::id("has space").is_err());
        assert!(check::id("nl\n").is_err());
        assert!(check::id(&"a".repeat(129)).is_err());
        assert!(check::id(&"a".repeat(128)).is_ok());
        assert!(check::id("0192f0c4-6a1b-7c3d-8e9f-0a1b2c3d4e5f").is_ok());
    }

    #[test]
    fn ts_wall_check_pins_the_shape() {
        assert!(check::ts_wall("2026-11-23T18:00:00.000Z").is_ok());
        // no milliseconds
        assert!(check::ts_wall("2026-11-23T18:00:00Z").is_err());
        // wrong suffix
        assert!(check::ts_wall("2026-11-23T18:00:00.000+08:00").is_err());
        // digit where a separator belongs
        assert!(check::ts_wall("2026-11-23_18:00:00.000Z").is_err());
    }

    #[test]
    fn error_codes_map_to_the_wire_table() {
        assert_eq!(
            ProtocolError::SchemaVersionUnsupported(2).code(),
            ErrorCode::SchemaVersionUnsupported
        );
        assert_eq!(ProtocolError::MissingType.code(), ErrorCode::MissingType);
        // the discard-cap case must map to payload_too_large, not invent a code
        assert_eq!(
            ProtocolError::FrameTooLarge(70 << 20).code(),
            ErrorCode::PayloadTooLarge
        );
    }
}
