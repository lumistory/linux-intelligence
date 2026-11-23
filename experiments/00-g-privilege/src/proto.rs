//! Wire types for the simulated privileged helper.
//!
//! The entire attacker-controlled input surface of the helper is this file:
//! a newline-delimited JSON object with a numeric `opcode` and a `args` bag.
//! There is deliberately no field that names a program, a shell, an
//! interpreter, or a raw command string.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum accepted frame. Larger frames are rejected, never truncated.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// If a peer streams a single unterminated line beyond this many bytes the
/// connection is dropped instead of buffered.
pub const DISCARD_CAP_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_ARG_BYTES: usize = 4096;
pub const MAX_READ_BYTES: u64 = 4096;

/// Incoming request. `opcode` is `Value` on purpose: a string opcode must be a
/// *classified* validation failure (`bad_opcode`), not a deserialisation error,
/// so it lands in the audit log with a reason.
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub opcode: Value,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub ok: bool,
    pub opcode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Response {
    pub fn ok(opcode: &str, data: Value) -> Self {
        Self {
            ok: true,
            opcode: opcode.to_owned(),
            data: Some(data),
            code: None,
            message: None,
        }
    }

    pub fn err(opcode: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            opcode: opcode.to_owned(),
            data: None,
            code: Some(code.to_owned()),
            message: Some(message.into()),
        }
    }
}

/// One '\n'-terminated JSON object, or the error code for a malformed frame.
pub fn parse_request(line: &[u8]) -> Result<Request, &'static str> {
    serde_json::from_slice::<Request>(line).map_err(|_| CODE_BAD_FRAME)
}

pub const CODE_BAD_FRAME: &str = "bad_frame";
pub const CODE_FRAME_TOO_LARGE: &str = "frame_too_large";
pub const CODE_DISCARD_EXCEEDED: &str = "discard_cap_exceeded";
pub const CODE_UNKNOWN_OPCODE: &str = "unknown_opcode";
pub const CODE_BAD_OPCODE: &str = "bad_opcode";
pub const CODE_PATH_ESCAPE: &str = "path_escape";
pub const CODE_PATH_INVALID: &str = "path_invalid";
pub const CODE_ARG_TOO_LARGE: &str = "arg_too_large";
pub const CODE_ARG_MISSING: &str = "arg_missing";
pub const CODE_NOT_AUTHORIZED: &str = "not_authorized";
pub const CODE_IO_ERROR: &str = "io_error";
pub const CODE_NOT_IMPLEMENTED: &str = "not_implemented_by_design";
