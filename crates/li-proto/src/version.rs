//! Schema versioning. v0 has **no negotiation** (`docs/ipc-protocol-v0.md` §5):
//! the receiver accepts exactly one version per frame and rejects the rest.

use crate::ProtocolError;

/// The only schema version this build speaks.
pub const CURRENT_SCHEMA_VERSION: u64 = 1;

/// A wire schema version.
///
/// Deliberately *not* an enum with a single variant: the value must survive
/// decoding so that the rejection can report **what the peer claimed**, and so
/// that a client can read a server's version off a frame and compare it
/// (`docs/ipc-protocol-v0.md` §5, "the server should advertise its version
/// somewhere a client can read before sending events").
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SchemaVersion(pub u64);

impl SchemaVersion {
    /// This build's version.
    pub const CURRENT: SchemaVersion = SchemaVersion(CURRENT_SCHEMA_VERSION);

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn is_current(self) -> bool {
        self.0 == CURRENT_SCHEMA_VERSION
    }
}

impl core::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Accepts `claimed` or rejects it, per frame.
///
/// Rejection is not advisory. A frame whose `schema_version` is greater than
/// ours may have moved or repurposed an envelope field; decoding it with the
/// v1 field set would silently write a wrong `sequence` or a wrong
/// `evidence_status` into an append-only journal, and history cannot be
/// rewritten to fix that (ADR 0003). So: reject, with a code the client can
/// act on, and never fall back to permissive parsing.
pub fn negotiate(claimed: SchemaVersion) -> Result<SchemaVersion, ProtocolError> {
    if claimed.is_current() {
        Ok(claimed)
    } else {
        Err(ProtocolError::SchemaVersionUnsupported(claimed.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_accepted() {
        assert_eq!(negotiate(SchemaVersion(1)), Ok(SchemaVersion::CURRENT));
        assert!(SchemaVersion::CURRENT.is_current());
    }

    #[test]
    fn a_future_version_is_rejected_with_its_own_number_in_the_error() {
        // The error must name the version the peer claimed, otherwise an
        // operator cannot tell "you are old" from "you are new".
        let err = negotiate(SchemaVersion(2)).unwrap_err();
        assert_eq!(err, ProtocolError::SchemaVersionUnsupported(2));
        assert!(err.to_string().contains("got 2"), "got: {err}");
    }

    #[test]
    fn version_zero_and_huge_versions_are_rejected_too() {
        // v0 is *not* "the same as v1 with defaults". It never existed on the
        // wire, so accepting it would be free.
        for v in [0, 3, u32::MAX as u64, u64::MAX] {
            assert_eq!(
                negotiate(SchemaVersion(v)).unwrap_err(),
                ProtocolError::SchemaVersionUnsupported(v)
            );
        }
    }
}
