//! Shared IPC / journal delivery helpers for `li` subcommands.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use li_journal::write::Session;
use li_journal::{Config, Journal, clock};
use li_proto::{
    CURRENT_SCHEMA_VERSION, Event, Ping, Pong, SchemaVersion, ServerMessage, SessionId,
    decode_server_message, encode_frame,
};

pub fn default_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn default_socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("linux-intelligence").join("daemon.sock")
}

pub fn resolve_journal_path(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("LI_JOURNAL_PATH") {
        return Ok(PathBuf::from(p));
    }
    li_journal::db::default_path()
        .ok_or_else(|| "cannot resolve journal path (set HOME or pass --journal)".to_string())
}

pub fn send_ping(sock: &Path, nonce: u64) -> Result<Pong, String> {
    let mut stream =
        UnixStream::connect(sock).map_err(|e| format!("connect {}: {e}", sock.display()))?;
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut frame = encode_frame(&Ping {
        schema_version: SchemaVersion(CURRENT_SCHEMA_VERSION),
        nonce,
    })
    .map_err(|e| e.to_string())?;
    frame.push('\n');
    stream
        .write_all(frame.as_bytes())
        .map_err(|e| format!("write ping: {e}"))?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|e| format!("read pong: {e}"))?;
    match decode_server_message(line.trim()).map_err(|e| e.to_string())? {
        ServerMessage::Pong(p) => Ok(*p),
        ServerMessage::Error(e) => Err(format!(
            "daemon rejected ping: {} ({:?})",
            e.message, e.code
        )),
        ServerMessage::Ack(_) => Err("unexpected Ack to Ping".to_string()),
    }
}

/// Delivers events via the running daemon UDS when reachable, falling back to
/// direct `Journal` persistence when `journal_fallback` is supplied or resolved.
pub fn deliver_events(
    sock: Option<&Path>,
    journal_path: Option<&Path>,
    session: Option<&Session>,
    events: &[Event],
) -> Result<(), String> {
    if let Some(s) = sock
        && s.exists()
        && let Ok(mut stream) = UnixStream::connect(s)
    {
        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        for ev in events {
            let mut line = ev.to_json_line().map_err(|e| e.to_string())?;
            line.push('\n');
            stream
                .write_all(line.as_bytes())
                .map_err(|e| format!("write event to daemon: {e}"))?;
        }
        let mut ping = encode_frame(&Ping {
            schema_version: SchemaVersion(CURRENT_SCHEMA_VERSION),
            nonce: u64::from(std::process::id()),
        })
        .map_err(|e| e.to_string())?;
        ping.push('\n');
        stream
            .write_all(ping.as_bytes())
            .map_err(|e| format!("write barrier ping: {e}"))?;
        let mut resp = String::new();
        BufReader::new(&stream)
            .read_line(&mut resp)
            .map_err(|e| format!("read barrier pong: {e}"))?;
        return match decode_server_message(resp.trim()).map_err(|e| e.to_string())? {
            ServerMessage::Pong(_) => Ok(()),
            ServerMessage::Error(e) => Err(format!("daemon error: {}", e.message)),
            ServerMessage::Ack(_) => Ok(()),
        };
    }

    let jpath = resolve_journal_path(journal_path)?;
    let mut journal = Journal::open(&jpath, Config::default()).map_err(|e| e.to_string())?;
    journal.migrate().map_err(|e| e.to_string())?;
    if let Some(sess) = session {
        journal.session(sess.clone());
    }
    for ev in events {
        journal.event(ev).map_err(|e| e.to_string())?;
    }
    let wm = journal.flush().map_err(|e| e.to_string())?;
    if wm.failed > 0 {
        return Err(format!(
            "journal rejected {} row(s): {:?}",
            wm.failed,
            journal.last_error()
        ));
    }
    Ok(())
}

pub fn make_session_id(prefix: &str) -> Result<SessionId, String> {
    let mono = clock::mono_ns().unwrap_or(0);
    SessionId::new(format!("{prefix}-{}-{mono}", std::process::id())).map_err(|e| e.to_string())
}
