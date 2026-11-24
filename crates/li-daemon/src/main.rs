//! `li-daemon`: Tokio daemon — UDS accept loops, bounded ingest, observers,
//! single SQLite writer thread, crash sentinel and restart reconciliation.
//!
//! TODO(phase1-batch2): UDS accept at `$XDG_RUNTIME_DIR/linux-intelligence/`,
//! dir 0700 / socket 0600, `SO_PEERCRED` uid allow-list, bounded ingest queue,
//! `DaemonRestarted` on sentinel detection, sequence-gap reconciliation.
fn main() {
    // Intentionally empty: this crate is a skeleton in batch 1.
}
