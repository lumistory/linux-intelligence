//! 0-H prototype: event deduplication, event correlation, credibility levels and
//! Operation Context over a SQLite store.
//!
//! Nothing here is production code. It exists to *measure* whether the Phase 0
//! design survives contact with duplicate events, out-of-order delivery, a
//! collision-prone process identity, and a killed writer.

pub mod confidence;
pub mod context;
pub mod db;
pub mod dedup;
pub mod envelope;
pub mod identity;
pub mod ordering;
pub mod redact;
pub mod relations;
pub mod util;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_links() {
        assert_eq!(super::identity::CLK_TCK, 100);
    }
}
