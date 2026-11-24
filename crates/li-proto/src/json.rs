//! Frame encoding: one JSON object per line, size limits enforced.
//!
//! The crate performs no I/O, so this module is the whole "framing" story:
//! [`encode_frame`] returns a line **without** its terminating `\n`, and
//! [`check_inbound_frame_len`] is what a reader calls before it buffers.
//!
//! JSON escaping guarantees that a serialised frame never contains a raw
//! `0x0A`: serde_json writes a newline inside a string as the two bytes
//! `\` `n`. A frame therefore cannot split itself, which is what makes
//! newline framing safe for shell command lines, file paths and heredocs.

use serde::Serialize;

use crate::event::Event;
use crate::{DISCARD_CAP_BYTES, MAX_FRAME_BYTES, ProtocolError};

/// Encodes any message as one frame, without the trailing newline.
///
/// Compact form, no whitespace: the frame is a wire object, not a pretty
/// document, and every byte here is charged to the shell hot path.
pub fn encode_frame<T: Serialize + ?Sized>(value: &T) -> Result<String, ProtocolError> {
    let s = serde_json::to_string(value).map_err(|e| ProtocolError::InvalidJson(e.to_string()))?;
    let len = s.len();
    if len > DISCARD_CAP_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::PayloadTooLarge(len));
    }
    Ok(s)
}

impl Event {
    /// One line of the wire format, without the trailing `\n`.
    ///
    /// Same bytes as [`encode_frame`]; this exists so that the hot path does not
    /// have to generic-instantiate the encoder per event type.
    pub fn to_json_line(&self) -> Result<String, ProtocolError> {
        encode_frame(self)
    }
}

/// Applies the inbound size policy to a buffered frame length.
///
/// Split from the reader on purpose: a reader needs this decision *before* it
/// has a complete frame, in order to stop buffering at the right point. Above
/// [`MAX_FRAME_BYTES`] the frame is discarded to its newline and the connection
/// survives; above [`DISCARD_CAP_BYTES`] the connection is closed, because an
/// unbounded discard is an unbounded buffer.
pub fn check_inbound_frame_len(len: usize) -> Result<(), ProtocolError> {
    if len > DISCARD_CAP_BYTES {
        Err(ProtocolError::FrameTooLarge(len))
    } else if len > MAX_FRAME_BYTES {
        Err(ProtocolError::PayloadTooLarge(len))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventId, EventType, EvidenceStatus, SessionId, Source};
    use crate::message::{ClientMessage, decode_client_message};

    fn sample() -> Event {
        let mut payload = serde_json::Map::new();
        payload.insert("command".into(), serde_json::json!("rm -rf build"));
        payload.insert("exit_status".into(), serde_json::json!(0));
        payload.insert("cwd".into(), serde_json::json!("<HOME>/src"));
        Event::new(
            EventId::new("0192f0c4-6a1b-7c3d-8e9f-0a1b2c3d4e5f").unwrap(),
            Source::ShellBash,
            Some(SessionId::new("sess-7f3a").unwrap()),
            42,
            1_234_567_890_123,
            "2026-11-23T18:00:00.000Z",
            EventType::new("CommandFinished").unwrap(),
            payload,
            EvidenceStatus::Declared,
        )
        .unwrap()
    }

    /// The whole point of a gold frame: if a field is renamed, reordered or
    /// dropped, this fails on the byte, not on a semantic "looks equal".
    #[test]
    fn gold_frame_is_byte_exact() {
        const GOLD: &str = concat!(
            r#"{"schema_version":1,"#,
            r#""event_id":"0192f0c4-6a1b-7c3d-8e9f-0a1b2c3d4e5f","#,
            r#""source":"shell.bash","#,
            r#""session_id":"sess-7f3a","#,
            r#""sequence":42,"#,
            r#""ts_mono_ns":1234567890123,"#,
            r#""ts_wall":"2026-11-23T18:00:00.000Z","#,
            r#""type":"CommandFinished","#,
            r#""payload":{"command":"rm -rf build","cwd":"<HOME>/src","exit_status":0},"#,
            r#""evidence_status":"declared"}"#
        );
        assert_eq!(sample().to_json_line().unwrap(), GOLD);
    }

    #[test]
    fn gold_frame_carries_no_raw_newline_and_no_trailing_newline() {
        let line = sample().to_json_line().unwrap();
        assert!(!line.contains('\n'), "frame must not contain a raw newline");
        assert!(
            !line.ends_with('\n'),
            "the caller appends the framing newline"
        );
    }

    #[test]
    fn a_command_line_with_a_real_newline_stays_on_one_frame() {
        // The risk this guards: an unescaped newline in the command text would
        // split one event into two frames and corrupt the stream for the rest
        // of the connection's life.
        let mut payload = serde_json::Map::new();
        payload.insert("command".into(), serde_json::json!("echo a\necho b"));
        let mut ev = sample();
        ev.payload = payload.clone();
        let line = ev.to_json_line().unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains(r#"\n"#));

        let ClientMessage::Event(back) = decode_client_message(&line).unwrap() else {
            panic!("expected an event")
        };
        assert_eq!(back.payload.get("command"), payload.get("command"));
    }

    #[test]
    fn gold_frame_decodes_back_to_the_same_event() {
        let ev = sample();
        let line = ev.to_json_line().unwrap();
        let ClientMessage::Event(back) = decode_client_message(&line).unwrap() else {
            panic!("expected an event")
        };
        assert_eq!(*back, ev);
    }

    #[test]
    fn frames_at_the_limit_are_accepted_and_one_byte_more_is_not() {
        assert_eq!(check_inbound_frame_len(MAX_FRAME_BYTES), Ok(()));
        assert_eq!(
            check_inbound_frame_len(MAX_FRAME_BYTES + 1),
            Err(ProtocolError::PayloadTooLarge(MAX_FRAME_BYTES + 1))
        );
        // 64 MiB is *inside* the discard cap, so it is a discardable frame, not
        // a connection kill: over MAX_FRAME_BYTES but the peer is not cut off.
        assert_eq!(
            check_inbound_frame_len(DISCARD_CAP_BYTES),
            Err(ProtocolError::PayloadTooLarge(DISCARD_CAP_BYTES))
        );
        assert_eq!(
            check_inbound_frame_len(DISCARD_CAP_BYTES + 1),
            Err(ProtocolError::FrameTooLarge(DISCARD_CAP_BYTES + 1))
        );
    }

    #[test]
    fn the_encoder_refuses_an_oversized_frame_instead_of_producing_one() {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "blob".into(),
            serde_json::json!("a".repeat(MAX_FRAME_BYTES + 16)),
        );
        let mut ev = sample();
        ev.payload = payload;
        assert!(matches!(
            ev.to_json_line().unwrap_err(),
            ProtocolError::PayloadTooLarge(_)
        ));
    }

    #[test]
    fn payload_keys_are_emitted_in_sorted_order_so_encoding_is_stable() {
        let mut a = serde_json::Map::new();
        a.insert("z".into(), serde_json::json!(1));
        a.insert("a".into(), serde_json::json!(2));
        let mut b = serde_json::Map::new();
        b.insert("a".into(), serde_json::json!(2));
        b.insert("z".into(), serde_json::json!(1));
        let mut ea = sample();
        ea.payload = a;
        let mut eb = sample();
        eb.payload = b;
        assert_eq!(ea.to_json_line().unwrap(), eb.to_json_line().unwrap());
    }
}
