//! `li-hook`: the process a shell adapter execs on the hot path.
//!
//! TODO(phase1-batch3): non-blocking connect/write (C1), bounded queue with an
//! explicit drop counter, `sequence` advanced on drop (C1/C2), and an inherited
//! sink FD rather than a shell-opened socket (0-A: no shell can open an
//! AF_UNIX socket by redirection; a write to a dead pipe kills the shell, C10).
fn main() {
    // Intentionally empty: this crate is a skeleton in batch 1.
}
