//! Startup reconciliation and Tokio UDS connection handler.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use li_journal::write::Session;
use li_journal::{Journal, clock};
use li_proto::{
    ClientMessage, DISCARD_CAP_BYTES, ErrorCode, ErrorFrame, Event, EventId, EventType,
    EvidenceStatus, MAX_FRAME_BYTES, ProtocolError, Source, check_inbound_frame_len,
    decode_client_message, encode_frame,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::state::{DaemonState, IngestOutcome};

pub const SOCKET_NAME: &str = "daemon.sock";
pub const MAX_CONCURRENT_CLIENTS: usize = 256;
/// Bounds how long an incomplete frame without `\n` may hold a connection (0-G T7).
pub const PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_millis(2_000);

#[derive(Debug)]
pub enum ServiceError {
    Io(std::io::Error),
    Protocol(ProtocolError),
    Journal(String),
    Unauthorized,
}

impl core::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ServiceError::Io(e) => write!(f, "io error: {e}"),
            ServiceError::Protocol(e) => write!(f, "protocol error: {e}"),
            ServiceError::Journal(e) => write!(f, "journal error: {e}"),
            ServiceError::Unauthorized => f.write_str("peer uid rejected"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<std::io::Error> for ServiceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<ProtocolError> for ServiceError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}

pub fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("linux-intelligence").join(SOCKET_NAME)
}

pub fn boot_id() -> Result<String, ServiceError> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_string())
}

/// Records daemon startup, reconciles sequence gaps against reported producer drops,
/// and seeds known sessions so future events never hit FK errors.
pub fn record_startup(
    journal: &Journal,
    id: &str,
    boot: &str,
) -> Result<Vec<String>, ServiceError> {
    let reader = journal
        .reader()
        .map_err(|e| ServiceError::Journal(e.to_string()))?;

    let sessions = reader
        .list_sessions(256)
        .map_err(|e| ServiceError::Journal(e.to_string()))?;
    let previous_daemon = sessions
        .iter()
        .find(|s| s.adapter == "li-daemon")
        .map(|s| (s.session_id.clone(), s.boot_id.clone()));

    let known_ids: Vec<String> = sessions.iter().map(|s| s.session_id.clone()).collect();

    let mut reconciled = Vec::new();
    let mut total_gap_span: u64 = 0;
    let mut reported_drops: u64 = 0;

    let drop_events = reader
        .query_events(None, None, Some("ProducerDropReported"), 256)
        .map_err(|e| ServiceError::Journal(e.to_string()))?;
    let mut drops_by_session: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    for ev in drop_events {
        if let Some(sid) = ev.payload.get("session_id").and_then(|v| v.as_str())
            && let Some(cnt) = ev.payload.get("dropped").and_then(|v| v.as_u64())
        {
            *drops_by_session.entry(sid.to_string()).or_insert(0) += cnt;
        }
    }

    for s in sessions.iter().take(64) {
        if s.adapter == "li-daemon" {
            continue;
        }
        let gaps = reader
            .sequence_gaps(&s.session_id)
            .map_err(|e| ServiceError::Journal(e.to_string()))?;
        let gap_count: u64 = gaps
            .iter()
            .map(|(a, b)| u64::try_from(b - a + 1).unwrap_or(0))
            .sum();
        let client_drops = drops_by_session.get(&s.session_id).copied().unwrap_or(0);
        if gap_count > 0 || client_drops > 0 {
            reconciled.push(s.session_id.clone());
            total_gap_span = total_gap_span.saturating_add(gap_count);
            reported_drops = reported_drops.saturating_add(client_drops);
        }
    }

    let now_wall = clock::wall_now_rfc3339_ms();
    let now_mono = clock::mono_ns().unwrap_or(0);

    journal.session(Session {
        session_id: id.into(),
        parent_session: None,
        adapter: "li-daemon".into(),
        adapter_version: Some(env!("CARGO_PKG_VERSION").into()),
        shell_pid: Some(i64::from(std::process::id())),
        started_wall: now_wall.clone(),
        started_mono_ns: i64::try_from(now_mono).unwrap_or(i64::MAX),
        cwd: None,
        host: None,
        boot_id: Some(boot.into()),
    });

    if let Some((previous_id, previous_boot)) = previous_daemon {
        let unexplained = total_gap_span.saturating_sub(reported_drops);
        let tail_status = if reported_drops > 0 {
            "client_drop_reported"
        } else {
            "not_determinable"
        };
        let mut payload = serde_json::Map::new();
        payload.insert(
            "previous_daemon_session".into(),
            serde_json::json!(previous_id),
        );
        payload.insert(
            "previous_boot".into(),
            serde_json::json!(previous_boot.unwrap_or_default()),
        );
        payload.insert("boot_id".into(), serde_json::json!(boot));
        payload.insert("reconciled_sessions".into(), serde_json::json!(reconciled));
        payload.insert(
            "observed_gap_count".into(),
            serde_json::json!(total_gap_span),
        );
        payload.insert(
            "reported_client_drops".into(),
            serde_json::json!(reported_drops),
        );
        payload.insert("unexplained_gaps".into(), serde_json::json!(unexplained));
        payload.insert(
            "producer_tail_status".into(),
            serde_json::json!(tail_status),
        );

        let event = Event::new(
            EventId::new(format!("daemon-restarted-{id}"))?,
            Source::Policy,
            None,
            0,
            now_mono,
            now_wall,
            EventType::new("DaemonRestarted")?,
            payload,
            EvidenceStatus::Declared,
        )?;
        journal
            .event(&event)
            .map_err(|e| ServiceError::Journal(e.to_string()))?;
    }

    journal
        .flush()
        .map_err(|e| ServiceError::Journal(e.to_string()))?;
    Ok(known_ids)
}

/// Binds a private `AF_UNIX` listener (`0700` parent dir, `0600` socket).
pub fn bind(runtime_dir: &Path) -> Result<UnixListener, ServiceError> {
    let dir = runtime_dir.join("linux-intelligence");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let path = dir.join(SOCKET_NAME);
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

enum LineOutcome {
    Frame(String),
    Eof,
    TooLong { fatal: bool, bytes: usize },
    PartialTimeout,
}

async fn read_bounded_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<LineOutcome, std::io::Error> {
    let mut buf = Vec::with_capacity(256);
    loop {
        let chunk = if buf.is_empty() {
            reader.fill_buf().await?
        } else {
            match tokio::time::timeout(PARTIAL_FRAME_TIMEOUT, reader.fill_buf()).await {
                Ok(res) => res?,
                Err(_) => return Ok(LineOutcome::PartialTimeout),
            }
        };

        if chunk.is_empty() {
            if buf.is_empty() {
                return Ok(LineOutcome::Eof);
            }
            return Ok(LineOutcome::PartialTimeout);
        }

        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            let new_len = buf.len() + pos;
            if check_inbound_frame_len(new_len).is_err() {
                let total = new_len;
                reader.consume(pos + 1);
                return Ok(LineOutcome::TooLong {
                    fatal: total > DISCARD_CAP_BYTES,
                    bytes: total,
                });
            }
            buf.extend_from_slice(&chunk[..pos]);
            reader.consume(pos + 1);
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(LineOutcome::Frame(s));
        }

        let len = chunk.len();
        if buf.len() + len > MAX_FRAME_BYTES {
            // Drain up to `\n` or `DISCARD_CAP_BYTES` so subsequent frames survive.
            let mut discarded = buf.len() + len;
            buf.clear();
            reader.consume(len);
            loop {
                let rest =
                    match tokio::time::timeout(PARTIAL_FRAME_TIMEOUT, reader.fill_buf()).await {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => return Err(e),
                        Err(_) => {
                            return Ok(LineOutcome::TooLong {
                                fatal: true,
                                bytes: discarded,
                            });
                        }
                    };
                if rest.is_empty() {
                    return Ok(LineOutcome::TooLong {
                        fatal: true,
                        bytes: discarded,
                    });
                }
                if let Some(pos) = rest.iter().position(|&b| b == b'\n') {
                    discarded += pos;
                    reader.consume(pos + 1);
                    return Ok(LineOutcome::TooLong {
                        fatal: discarded > DISCARD_CAP_BYTES,
                        bytes: discarded,
                    });
                }
                discarded += rest.len();
                let rlen = rest.len();
                reader.consume(rlen);
                if discarded > DISCARD_CAP_BYTES {
                    return Ok(LineOutcome::TooLong {
                        fatal: true,
                        bytes: discarded,
                    });
                }
            }
        }
        buf.extend_from_slice(chunk);
        reader.consume(len);
    }
}

async fn send_error(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    err: ErrorFrame,
) -> Result<(), std::io::Error> {
    if let Ok(mut encoded) = encode_frame(&err) {
        encoded.push('\n');
        writer.write_all(encoded.as_bytes()).await?;
    }
    Ok(())
}

/// Serves one authenticated peer connection until EOF, terminal protocol error,
/// or partial-frame timeout.
pub async fn serve_connection(
    stream: UnixStream,
    state: Arc<DaemonState>,
    expected_uid: u32,
    _permit: Option<OwnedSemaphorePermit>,
) -> Result<(), ServiceError> {
    let creds = rustix::net::sockopt::socket_peercred(&stream).map_err(std::io::Error::from)?;
    let (read_half, mut write_half) = stream.into_split();

    if creds.uid.as_raw() != expected_uid {
        let _ = send_error(
            &mut write_half,
            ErrorFrame::new(
                ErrorCode::PeerUidRejected,
                format!("uid {} is not authorized", creds.uid.as_raw()),
            ),
        )
        .await;
        return Err(ServiceError::Unauthorized);
    }

    let mut reader = BufReader::new(read_half);
    loop {
        let line = match read_bounded_line(&mut reader).await {
            Ok(LineOutcome::Eof) => return Ok(()),
            Ok(LineOutcome::PartialTimeout) => {
                state.note_rejected();
                let _ = send_error(
                    &mut write_half,
                    ErrorFrame::new(
                        ErrorCode::InvalidJson,
                        "unterminated frame timed out before newline",
                    ),
                )
                .await;
                return Ok(());
            }
            Ok(LineOutcome::TooLong { fatal, bytes }) => {
                state.note_rejected();
                let err = if fatal {
                    ProtocolError::FrameTooLarge(bytes)
                } else {
                    ProtocolError::PayloadTooLarge(bytes)
                };
                let frame = if fatal {
                    ErrorFrame::new(ErrorCode::FrameTooLarge, err.to_string())
                } else {
                    ErrorFrame::from_error(&err)
                };
                let _ = send_error(&mut write_half, frame).await;
                if fatal {
                    return Err(ServiceError::Protocol(err));
                }
                continue;
            }
            Ok(LineOutcome::Frame(s)) => s,
            Err(e) => return Err(ServiceError::Io(e)),
        };

        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            continue;
        }

        match decode_client_message(trimmed) {
            Ok(ClientMessage::Event(ev)) => {
                let mut pending = Some(*ev);
                for _ in 0..100 {
                    let Some(ev_now) = pending.take() else {
                        break;
                    };
                    match state.ingest_event(ev_now.clone()) {
                        IngestOutcome::Stored | IngestOutcome::Duplicate => break,
                        IngestOutcome::Backpressure => {
                            // Spec §10: pause draining this socket so backpressure
                            // propagates instead of silently dropping accepted bytes.
                            pending = Some(ev_now);
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                        IngestOutcome::Failed(e) => {
                            let _ = send_error(
                                &mut write_half,
                                ErrorFrame::new(ErrorCode::Other, format!("ingest rejected: {e}")),
                            )
                            .await;
                            break;
                        }
                    }
                }
                if pending.is_some() {
                    state.note_rejected();
                    let _ = send_error(
                        &mut write_half,
                        ErrorFrame::new(
                            ErrorCode::Other,
                            "ingest rejected: writer backpressure timeout",
                        ),
                    )
                    .await;
                }
            }
            Ok(ClientMessage::DropReport(report)) => {
                if let Err(e) = state.ingest_drop_report(report) {
                    let _ = send_error(
                        &mut write_half,
                        ErrorFrame::new(ErrorCode::Other, format!("drop report rejected: {e}")),
                    )
                    .await;
                }
            }
            Ok(ClientMessage::Ping(ping)) => {
                let journal = Arc::clone(&state.journal);
                let flush_res = tokio::task::spawn_blocking(move || journal.flush()).await;
                if let Err(e) = flush_res.map_err(|e| ServiceError::Journal(e.to_string()))? {
                    return Err(ServiceError::Journal(e.to_string()));
                }
                let pong = state.build_pong(ping.nonce);
                let mut out = encode_frame(&pong)?;
                out.push('\n');
                write_half.write_all(out.as_bytes()).await?;
            }
            Err(e) => {
                state.note_rejected();
                let close = e.code().closes_connection();
                let _ = send_error(&mut write_half, ErrorFrame::from_error(&e)).await;
                if close {
                    return Err(ServiceError::Protocol(e));
                }
            }
        }
    }
}

/// Runs the listener accept loop until `shutdown` fires, then flushes the journal.
pub async fn run_accept_loop(
    listener: UnixListener,
    state: Arc<DaemonState>,
    expected_uid: u32,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ServiceError> {
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CLIENTS));
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let Ok(permit) = sem.clone().try_acquire_owned() else {
                            // Connection limit reached; drop socket without stalling accept.
                            drop(stream);
                            continue;
                        };
                        let st = Arc::clone(&state);
                        tokio::spawn(async move {
                            let _ = serve_connection(stream, st, expected_uid, Some(permit)).await;
                        });
                    }
                    Err(e) => return Err(ServiceError::Io(e)),
                }
            }
        }
    }
    let journal = Arc::clone(&state.journal);
    let _ = tokio::task::spawn_blocking(move || journal.flush()).await;
    Ok(())
}
