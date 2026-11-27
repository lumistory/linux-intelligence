//! Library surface for `li-daemon` so integration tests can drive the real
//! async UDS accept loop and startup reconciliation directly.

pub mod correlation;
pub mod helper;
pub mod observers;
pub mod pty;
pub mod recovery;
pub mod server;
pub mod state;
