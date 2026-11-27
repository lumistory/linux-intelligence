//! Request framing, bounded transport, and loss accounting for shell adapters.

use std::io::{BufRead, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

use li_proto::{
    CURRENT_SCHEMA_VERSION, DropReport, Event, EventId, EventType, EvidenceStatus, Ping,
    ProtocolError, SchemaVersion, SessionId, Source, encode_frame,
};

/// Bound the amount of queued work when the daemon falls behind.
pub const QUEUE_CAP_DEFAULT: usize = 64;
/// The time half of C1.
pub const SEND_TIMEOUT: Duration = Duration::from_millis(5);
/// Bound a connect attempt as well as a write.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(5);
pub const SESSION_ENV: &str = "LI_SESSION_ID";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    Bash,
    Zsh,
    Fish,
}

impl Adapter {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bash" => Some(Adapter::Bash),
            "zsh" => Some(Adapter::Zsh),
            "fish" => Some(Adapter::Fish),
            _ => None,
        }
    }

    pub fn source(self) -> Source {
        match self {
            Adapter::Bash => Source::ShellBash,
            Adapter::Zsh => Source::ShellZsh,
            Adapter::Fish => Source::ShellFish,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Adapter::Bash => "bash",
            Adapter::Zsh => "zsh",
            Adapter::Fish => "fish",
        }
    }
}

/// Reject fields this client version does not understand.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(rename = "type")]
    pub event_type: String,
    pub ts_wall: String,
    #[serde(default)]
    pub payload: serde_json::Map<String, serde_json::Value>,
}

/// Counters reconcile every accepted event; malformed input takes no sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Report {
    pub accepted: u64,
    pub delivered: u64,
    pub dropped: u64,
    /// Invalid input does not take a sequence number.
    pub malformed: u64,
    pub last_sequence: Option<u64>,
}

impl Report {
    pub fn receiver_gap(&self) -> u64 {
        self.last_sequence
            .map_or(0, |l| (l + 1).saturating_sub(self.delivered))
    }

    /// Gap implied by the last sequence and delivered event count.
    pub fn as_line(&self, adapter: Adapter) -> String {
        format!(
            "li-hook[{}] accepted={} delivered={} dropped={} malformed={} last_sequence={} receiver_gap={}",
            adapter.name(),
            self.accepted,
            self.delivered,
            self.dropped,
            self.malformed,
            self.last_sequence
                .map_or_else(|| "none".into(), |s| s.to_string()),
            self.receiver_gap()
        )
    }
}

/// Bounded pending frames. A dropped frame still consumes its sequence number.
#[derive(Debug)]
pub struct Queue {
    cap: usize,
    pending: std::collections::VecDeque<String>,
    next_seq: u64,
    delivered: u64,
    dropped: u64,
    malformed: u64,
}

impl Queue {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Queue {
            cap,
            pending: std::collections::VecDeque::with_capacity(cap),
            next_seq: 0,
            delivered: 0,
            dropped: 0,
            malformed: 0,
        }
    }

    /// `false` means it did not fit; the number is spent either way (C1).
    pub fn accept(&mut self, frame: String) -> bool {
        self.next_seq += 1;
        if self.pending.len() >= self.cap {
            self.dropped += 1;
            return false;
        }
        self.pending.push_back(frame);
        true
    }

    /// Spends a number for a frame that could not be built or encoded at all.
    pub fn accept_uncodable(&mut self) {
        self.next_seq += 1;
        self.dropped += 1;
    }

    pub fn note_malformed(&mut self) {
        self.malformed += 1;
    }

    pub fn pop(&mut self) -> Option<String> {
        self.pending.pop_front()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_seq
    }

    pub fn report(&self) -> Report {
        Report {
            accepted: self.next_seq,
            delivered: self.delivered,
            dropped: self.dropped,
            malformed: self.malformed,
            last_sequence: self.next_seq.checked_sub(1),
        }
    }
}

/// Build an event with caller-assigned sequence and declared evidence.
pub fn build_event(
    req: &Request,
    adapter: Adapter,
    session: &SessionId,
    sequence: u64,
) -> Result<Event, ProtocolError> {
    Event::new(
        EventId::new(event_id(session.as_str(), sequence))?,
        adapter.source(),
        Some(session.clone()),
        sequence,
        0,
        req.ts_wall.clone(),
        EventType::new(req.event_type.clone())?,
        req.payload.clone(),
        EvidenceStatus::Declared,
    )
}

/// Stable per-session deduplication key, within the protocol length limit.
pub fn event_id(session: &str, sequence: u64) -> String {
    let head: String = session.chars().take(96).collect();
    format!("{head}-{sequence}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkError {
    /// `ENOENT` when no daemon listens.
    Connect(String),
    ConnectTimeout,
    /// `EPIPE`, `ECONNRESET`, or our own `SO_SNDTIMEO`.
    Write(String),
    Timeout,
    Dead,
}

impl core::fmt::Display for SinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SinkError::Connect(e) => write!(f, "connect failed: {e}"),
            SinkError::ConnectTimeout => {
                write!(f, "connect timed out ({}ms)", CONNECT_TIMEOUT.as_millis())
            }
            SinkError::Write(e) => write!(f, "write failed: {e}"),
            SinkError::Timeout => {
                write!(
                    f,
                    "frame did not leave within {}ms",
                    SEND_TIMEOUT.as_millis()
                )
            }
            SinkError::Dead => f.write_str("sink already retired"),
        }
    }
}

impl std::error::Error for SinkError {}

/// A trait so the drop path is tested by a sink that fails on a chosen frame.
pub trait FrameSink {
    fn write_frame(&mut self, line: &str) -> Result<(), SinkError>;
    fn is_live(&self) -> bool;
}

enum Inner {
    /// Launcher-supplied socket on stdout (C10 / P1-08). The launcher, not the
    /// shell or this process, owns the `connect(2)`.
    Inherited(std::io::Stdout),
    /// Our own connection, bounded by a real `SO_SNDTIMEO`.
    Stream(UnixStream),
    Dead,
}

/// The outbound side. One per process, never re-created after a failure.
///
/// Both shapes set a 5 ms kernel send timeout. The inherited path can do that
/// despite `unsafe_code = "forbid"` because stdout is a `Stdout` handle whose
/// `AsFd` is safe; `SO_SNDTIMEO` then applies to the socket the launcher put
/// there. A self-dial remains only for diagnostics/tests (`--sink=socket`), never
/// as the default. A failure is **permanent and silent**: retrying turns a stall
/// into a shell-side stall (0-C T4: a blocking write to a `SIGSTOP`ed daemon had
/// not returned after 2 s).
pub struct Sink {
    inner: Inner,
    pub written: u64,
}

impl core::fmt::Debug for Sink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self.inner {
            Inner::Inherited(_) => "inherited",
            Inner::Stream(_) => "socket",
            Inner::Dead => "dead",
        };
        write!(f, "Sink({kind}, written={})", self.written)
    }
}

impl Sink {
    pub fn dead() -> Self {
        Sink {
            inner: Inner::Dead,
            written: 0,
        }
    }

    /// Adopt the launcher's socket on stdout and bound every write to 5 ms.
    ///
    /// `ENOTSOCK` is deliberate: a terminal or an ordinary pipe is not a sink.
    /// Without this check li-hook would write protocol frames to a user's tty.
    pub fn inherited() -> Result<Self, SinkError> {
        let out = std::io::stdout();
        if out.is_terminal() {
            return Err(SinkError::Write(
                "inherited sink is a terminal, not a launcher socket".into(),
            ));
        }
        rustix::net::sockopt::set_socket_timeout(
            &out,
            rustix::net::sockopt::Timeout::Send,
            Some(SEND_TIMEOUT),
        )
        .map_err(|e| SinkError::Write(format!("SO_SNDTIMEO on inherited sink: {e}")))?;
        Ok(Sink {
            inner: Inner::Inherited(out),
            written: 0,
        })
    }

    /// Dials `path` with a bounded connect, then `SO_SNDTIMEO = 5 ms`.
    pub fn connect(path: &Path) -> Result<Self, SinkError> {
        let stream = connect_with_deadline(path, CONNECT_TIMEOUT)?;
        stream
            .set_write_timeout(Some(SEND_TIMEOUT))
            .map_err(|e| SinkError::Write(e.to_string()))?;
        Ok(Sink {
            inner: Inner::Stream(stream),
            written: 0,
        })
    }

    /// Send one protocol Ping to verify an inherited sink before event writes.
    pub fn probe(&mut self, nonce: u64) -> Result<(), SinkError> {
        if matches!(self.inner, Inner::Dead) {
            return Err(SinkError::Dead);
        }
        let line = encode_frame(&Ping {
            schema_version: SchemaVersion(CURRENT_SCHEMA_VERSION),
            nonce,
        })
        .map_err(|e| SinkError::Write(e.to_string()))?;
        self.write_frame(&line)
    }
}

impl FrameSink for Sink {
    fn write_frame(&mut self, line: &str) -> Result<(), SinkError> {
        let result = match &mut self.inner {
            Inner::Dead => Err(SinkError::Dead),
            // The kernel holds SO_SNDTIMEO for us: no thread, no deadline.
            Inner::Stream(s) => s
                .write_all(line.as_bytes())
                .and_then(|()| s.write_all(b"\n"))
                .and_then(|()| s.flush())
                .map_err(|e| SinkError::Write(e.to_string())),
            Inner::Inherited(s) => s
                .write_all(line.as_bytes())
                .and_then(|()| s.write_all(b"\n"))
                .and_then(|()| s.flush())
                .map_err(|e| SinkError::Write(e.to_string())),
        };
        match result {
            Ok(()) => {
                self.written += 1;
                Ok(())
            }
            // C1/C10: one failure retires the sink. A sink that "comes back" is
            // one the shell can be stalled by twice.
            Err(e) => {
                self.inner = Inner::Dead;
                Err(e)
            }
        }
    }

    fn is_live(&self) -> bool {
        !matches!(self.inner, Inner::Dead)
    }
}

/// `unix connect` normally completes without a handshake, but a listener with a
/// full accept backlog blocks - the 0-C T4 class of hang. The thread bounds that
/// wait, and a timed-out connect is never retried.
fn connect_with_deadline(path: &Path, timeout: Duration) -> Result<UnixStream, SinkError> {
    let (tx, rx) = channel();
    let owned: PathBuf = path.to_path_buf();
    std::thread::Builder::new()
        .name("li-connect".into())
        .spawn(move || {
            let _ = tx.send(UnixStream::connect(&owned).map_err(|e| e.to_string()));
        })
        .map_err(|e| SinkError::Connect(e.to_string()))?;
    match rx.recv_timeout(timeout) {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(SinkError::Connect(e)),
        Err(_) => Err(SinkError::ConnectTimeout),
    }
}

/// Consume request lines and account for every accepted, malformed, or dropped event.
pub fn ingest<R: BufRead, S: FrameSink>(
    mut input: R,
    sink: &mut S,
    mut queue: Queue,
    adapter: Adapter,
    session: &SessionId,
) -> Report {
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Request>(trimmed) else {
            queue.note_malformed();
            continue;
        };
        let seq = queue.next_sequence();
        match build_event(&req, adapter, session, seq).and_then(|ev| encode_frame(&ev)) {
            Ok(frame) => {
                queue.accept(frame);
            }
            Err(_) => queue.accept_uncodable(),
        }
        // Stops at the first failure: the sink is then retired, so every remaining
        // frame is a known, counted drop.
        while let Some(frame) = queue.pop() {
            match sink.write_frame(&frame) {
                Ok(()) => queue.delivered += 1,
                Err(_) => {
                    queue.dropped += queue.len() as u64 + 1;
                    while queue.pop().is_some() {}
                    break;
                }
            }
        }
    }
    let report = queue.report();
    if report.dropped > 0 && sink.is_live() {
        let drop_frame = DropReport {
            schema_version: SchemaVersion(CURRENT_SCHEMA_VERSION),
            session_id: session.clone(),
            dropped: report.dropped,
            last_sequence: report.last_sequence,
            reason: "queue_full".to_string(),
        };
        if let Ok(encoded) = encode_frame(&drop_frame) {
            let _ = sink.write_frame(&encoded);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(i: u64) -> String {
        format!(
            r#"{{"type":"CommandFinished","ts_wall":"2026-11-24T18:00:00.000Z","payload":{{"command":"echo {i}","exit_status":0}}}}"#
        )
    }

    struct Counting {
        written: u64,
    }

    impl FrameSink for Counting {
        fn write_frame(&mut self, line: &str) -> Result<(), SinkError> {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "not JSON: {line}"
            );
            self.written += 1;
            Ok(())
        }
        fn is_live(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_drop_burns_the_sequence_number_so_the_gap_stays_computable() {
        // The single most load-bearing line in the crate (C1/C2).
        let mut q = Queue::new(1);
        assert!(q.accept("a".into()));
        assert!(
            !q.accept("b".into()),
            "capacity 1 must refuse the second frame"
        );
        assert!(!q.accept("c".into()));
        q.delivered += 1;
        let r = q.report();
        assert_eq!(
            (r.accepted, r.last_sequence),
            (3, Some(2)),
            "every frame takes a number"
        );
        // The receiver sees only sequence 0, so it computes 3 - 1 = 2.
        assert_eq!((r.receiver_gap(), r.dropped), (2, 2));
        assert_eq!(r.accepted, r.delivered + r.dropped);
    }

    #[test]
    fn one_dropped_middle_event_produces_exactly_one_receiver_gap() {
        let mut q = Queue::new(1);
        assert!(q.accept("seq0".into()));
        assert!(!q.accept("seq1".into()));
        assert_eq!(q.next_sequence(), 2);
        assert_eq!(q.pop().as_deref(), Some("seq0"));
        assert!(q.accept("seq2".into()));
        let received = [0u64, 2];
        let receiver_gap = received.last().unwrap() + 1 - received.len() as u64;
        assert_eq!(receiver_gap, 1);
        assert_eq!(q.report().dropped, 1);
    }

    #[test]
    fn a_bad_line_takes_no_sequence_but_an_unencodable_frame_still_does() {
        let s = SessionId::new("li-t").unwrap();
        // A working sink, so `delivered` reflects what actually got out: with a dead
        // one a malformed line would be indistinguishable from a real loss.
        let mut ok = Counting { written: 0 };
        let bad = format!("{}\nnot json\n{}\n", req(0), req(1));
        let r = ingest(
            std::io::Cursor::new(bad),
            &mut ok,
            Queue::new(8),
            Adapter::Bash,
            &s,
        );
        assert_eq!(
            (r.malformed, r.accepted, r.delivered, r.dropped),
            (1, 2, 2, 0)
        );
        assert_eq!(r.receiver_gap(), 0, "a bad line must not fake a gap");
        assert_eq!(ok.written, 2, "and only valid lines reach the sink");

        let mut dead = Sink::dead();
        let big = format!(
            r#"{{"type":"CommandFinished","ts_wall":"2026-11-24T18:00:00.000Z","payload":{{"blob":"{}"}}}}"#,
            "a".repeat(li_proto::MAX_FRAME_BYTES + 16)
        );
        let r = ingest(
            std::io::Cursor::new(big),
            &mut dead,
            Queue::new(8),
            Adapter::Bash,
            &s,
        );
        assert_eq!(
            (r.accepted, r.delivered, r.dropped),
            (1, 0, 1),
            "still burns a number"
        );
    }

    #[test]
    fn a_request_cannot_smuggle_an_envelope_field_and_events_stay_declared() {
        // A request carrying `sequence` would let a shell rewrite the loss detector.
        let smuggled =
            r#"{"type":"CommandFinished","ts_wall":"2026-11-24T18:00:00.000Z","sequence":99}"#;
        assert!(serde_json::from_str::<Request>(smuggled).is_err());

        let r: Request = serde_json::from_str(&req(0)).unwrap();
        let ev = build_event(&r, Adapter::Zsh, &SessionId::new("li-t").unwrap(), 3).unwrap();
        assert_eq!(
            (ev.sequence, ev.ts_mono_ns),
            (3, 0),
            "0-A: no shell has CLOCK_MONOTONIC"
        );
        assert_eq!(ev.evidence_status, EvidenceStatus::Declared);
        assert!(li_proto::decode_client_message(&ev.to_json_line().unwrap()).is_ok());
        assert_ne!(event_id("s", 7), event_id("s", 8));
        assert!(
            EventId::new(event_id(&"x".repeat(200), u64::MAX)).is_ok(),
            "id <= 128 bytes"
        );
    }

    #[test]
    fn a_missing_socket_fails_fast_and_a_dead_sink_stays_dead() {
        let started = std::time::Instant::now();
        let err = Sink::connect(Path::new("/nonexistent/li-test-no.sock")).unwrap_err();
        assert!(matches!(err, SinkError::Connect(_)), "got {err:?}");
        assert!(started.elapsed() < CONNECT_TIMEOUT * 4);

        let mut s = Sink::dead();
        assert!(!s.is_live());
        assert_eq!(s.write_frame("{}"), Err(SinkError::Dead));
    }
}
