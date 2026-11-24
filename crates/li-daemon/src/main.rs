use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use li_journal::{
    Config, Journal,
    write::{Session, Write as JournalWrite},
};
use li_proto::{
    ClientMessage, ProtocolError, check_inbound_frame_len, decode_client_message, encode_frame,
};

pub const SOCKET_NAME: &str = "daemon.sock";

#[derive(Debug)]
pub enum ServiceError {
    Io(std::io::Error),
    Protocol(ProtocolError),
    Unauthorized,
}

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

fn boot_id() -> Result<String, ServiceError> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_string())
}

fn record_startup(journal: &Journal, id: &str, boot: &str) -> Result<(), ServiceError> {
    let reader = journal
        .reader()
        .map_err(|e| ServiceError::Io(std::io::Error::other(e)))?;
    let previous: Option<(String, Option<String>)> = match reader.connection().query_row(
        "SELECT session_id, boot_id FROM sessions ORDER BY started_mono_ns DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ) {
        Ok(value) => Some(value),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(ServiceError::Io(std::io::Error::other(e))),
    };
    let now = timestamp();
    journal.submit(JournalWrite::Session(Session {
        session_id: id.into(),
        parent_session: None,
        adapter: "li-daemon".into(),
        adapter_version: Some(env!("CARGO_PKG_VERSION").into()),
        shell_pid: Some(std::process::id() as i64),
        started_wall: now.clone(),
        started_mono_ns: li_journal::clock::mono_ns().unwrap_or(0) as i64,
        cwd: None,
        host: None,
        boot_id: Some(boot.into()),
    }));
    if let Some((previous_id, previous_boot)) = previous {
        let gaps = reader
            .sequence_gaps(&previous_id)
            .map_err(|e| ServiceError::Io(std::io::Error::other(e)))?;
        let mut payload = serde_json::Map::new();
        payload.insert(
            "previous_boot".into(),
            serde_json::json!(previous_boot.unwrap_or_default()),
        );
        payload.insert("boot_id".into(), serde_json::json!(boot));
        payload.insert(
            "reconciled_sessions".into(),
            serde_json::json!([previous_id]),
        );
        payload.insert("unexplained_gaps".into(), serde_json::json!(gaps.len()));
        // The hook reports drops only to its own stderr. Its protocol has no
        // durable producer watermark, so the journal cannot classify a tail.
        payload.insert(
            "producer_tail_status".into(),
            serde_json::json!("not_determinable"),
        );
        let event = li_proto::Event::new(
            li_proto::EventId::new(format!("daemon-restarted-{id}"))?,
            li_proto::Source::Policy,
            None,
            0,
            li_journal::clock::mono_ns().unwrap_or(0),
            now,
            li_proto::EventType::new("DaemonRestarted")?,
            payload,
            li_proto::EvidenceStatus::Declared,
        )?;
        journal
            .event(&event)
            .map_err(|e| ServiceError::Io(std::io::Error::other(e)))?;
    }
    journal
        .flush()
        .map_err(|e| ServiceError::Io(std::io::Error::other(e)))?;
    Ok(())
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let secs = millis / 1000;
    let days = (secs / 86400) as i64;
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60,
        millis % 1000
    )
}

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

pub fn serve_one(stream: UnixStream, journal: &Journal, owner: u32) -> Result<(), ServiceError> {
    let creds = rustix::net::sockopt::socket_peercred(&stream).map_err(std::io::Error::from)?;
    if creds.uid.as_raw() != owner {
        return Err(ServiceError::Unauthorized);
    }
    let mut stream = stream;
    loop {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if stream.read(&mut byte)? == 0 {
                if line.is_empty() {
                    return Ok(());
                }
                break;
            }
            if byte[0] == b'\n' {
                break;
            }
            check_inbound_frame_len(line.len() + 1)?;
            line.push(byte[0]);
        }
        if line.is_empty() && byte[0] == 0 {
            return Ok(());
        }
        let text = std::str::from_utf8(&line).map_err(|e| {
            ServiceError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        })?;
        let message = decode_client_message(text.trim_end_matches('\r'))?;
        match message {
            ClientMessage::Event(event) => {
                journal
                    .event(&event)
                    .map_err(|e| ServiceError::Io(std::io::Error::other(e)))?;
            }
            ClientMessage::Ping(ping) => {
                journal
                    .flush()
                    .map_err(|e| ServiceError::Io(std::io::Error::other(e)))?;
                let pong = li_proto::Pong {
                    schema_version: ping.schema_version,
                    nonce: ping.nonce,
                    watermarks: Vec::new(),
                    counts: li_proto::Counts::default(),
                    server_schema_version: li_proto::SchemaVersion(
                        li_proto::CURRENT_SCHEMA_VERSION,
                    ),
                };
                let mut out = encode_frame(&pong).map_err(ServiceError::Protocol)?;
                out.push('\n');
                stream.write_all(out.as_bytes())?;
            }
        }
    }
}

fn main() {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let listener = bind(&runtime).expect("bind daemon socket");
    let journal_path = runtime.join("linux-intelligence").join("events.db");
    let mut journal = Journal::open(&journal_path, Config::default()).expect("open journal");
    journal.migrate().expect("migrate journal");
    let boot = boot_id().expect("read boot id");
    let startup_id = format!(
        "daemon-{}-{}",
        std::process::id(),
        li_journal::clock::mono_ns().unwrap_or(0)
    );
    record_startup(&journal, &startup_id, &boot).expect("record startup");
    let journal = Arc::new(journal);
    let owner = rustix::process::getuid().as_raw();
    for stream in listener.incoming().flatten() {
        let journal = journal.clone();
        thread::spawn(move || {
            let _ = serve_one(stream, &journal, owner);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use li_proto::{Ping, SchemaVersion};
    use std::io::BufRead;
    use std::os::unix::fs::MetadataExt;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("li-daemon-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn journal(path: &Path) -> Journal {
        let mut journal = Journal::open(path, Config::default()).unwrap();
        journal.migrate().unwrap();
        journal
    }

    #[test]
    fn socket_and_directory_permissions_are_private() {
        let root = tempdir("perms");
        let listener = bind(&root).unwrap();
        let dir = root.join("linux-intelligence");
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(socket_path(&root))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().uid(),
            fs::metadata(&root).unwrap().uid()
        );
        drop(listener);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn peer_uid_mismatch_is_rejected() {
        let root = tempdir("uid");
        let j = journal(&root.join("journal.db"));
        let (server, _client) = UnixStream::pair().unwrap();
        assert!(matches!(
            serve_one(server, &j, u32::MAX),
            Err(ServiceError::Unauthorized)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_records_boot_id_and_restart_marker() {
        let root = tempdir("startup");
        let j = journal(&root.join("journal.db"));
        record_startup(&j, "daemon-test-1", "boot-a").unwrap();
        record_startup(&j, "daemon-test-2", "boot-a").unwrap();
        let r = j.reader().unwrap();
        assert_eq!(r.count("sessions").unwrap(), 2);
        assert_eq!(r.count("events").unwrap(), 1);
        let marker = r
            .connection()
            .query_row(
                "SELECT payload FROM events WHERE type = 'DaemonRestarted'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let marker: serde_json::Value = serde_json::from_str(&marker).unwrap();
        assert_eq!(marker["unexplained_gaps"], 0);
        assert_eq!(marker["producer_tail_status"], "not_determinable");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn restart_reports_observed_gaps_but_does_not_claim_tail_completeness() {
        let root = tempdir("tail-gap");
        let j = journal(&root.join("journal.db"));
        let now = timestamp();
        j.submit(JournalWrite::Session(Session {
            session_id: "producer-a".into(),
            parent_session: None,
            adapter: "bash".into(),
            adapter_version: None,
            shell_pid: None,
            started_wall: now.clone(),
            started_mono_ns: 1,
            cwd: None,
            host: None,
            boot_id: Some("boot-a".into()),
        }));
        for sequence in [0, 2] {
            let event = li_proto::Event::new(
                li_proto::EventId::new(format!("producer-a-{sequence}")).unwrap(),
                li_proto::Source::ShellBash,
                Some(li_proto::SessionId::new("producer-a").unwrap()),
                sequence,
                sequence,
                now.clone(),
                li_proto::EventType::new("CommandFinished").unwrap(),
                serde_json::Map::new(),
                li_proto::EvidenceStatus::Declared,
            )
            .unwrap();
            j.event(&event).unwrap();
        }
        j.flush().unwrap();
        record_startup(&j, "daemon-test-restarted", "boot-a").unwrap();
        let r = j.reader().unwrap();
        let marker = r
            .connection()
            .query_row(
                "SELECT payload FROM events WHERE type = 'DaemonRestarted'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let marker: serde_json::Value = serde_json::from_str(&marker).unwrap();
        assert_eq!(marker["unexplained_gaps"], 1);
        assert_eq!(marker["producer_tail_status"], "not_determinable");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ping_is_a_flush_barrier_and_handshake_round_trips() {
        let root = tempdir("ping");
        let j = journal(&root.join("journal.db"));
        let (server, mut client) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            serve_one(server, &j, rustix::process::getuid().as_raw()).unwrap()
        });
        let ping = Ping {
            schema_version: SchemaVersion::CURRENT,
            nonce: 12345,
        };
        let mut bytes = li_proto::encode_frame(&ping).unwrap();
        bytes.push('\n');
        client.write_all(bytes.as_bytes()).unwrap();
        let mut response = String::new();
        std::io::BufReader::new(&mut client)
            .read_line(&mut response)
            .unwrap();
        assert!(
            matches!(li_proto::decode_server_message(response.trim()).unwrap(), li_proto::ServerMessage::Pong(p) if p.nonce == 12345)
        );
        drop(client);
        worker.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn overlong_frame_is_rejected_at_protocol_limit() {
        let root = tempdir("length");
        let j = journal(&root.join("journal.db"));
        let (server, mut client) = UnixStream::pair().unwrap();
        let worker =
            thread::spawn(move || serve_one(server, &j, rustix::process::getuid().as_raw()));
        let line = format!("{}\n", "x".repeat(li_proto::MAX_FRAME_BYTES + 1));
        client.write_all(line.as_bytes()).unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Err(ServiceError::Protocol(ProtocolError::PayloadTooLarge(_)))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_clients_write_through_one_journal_and_rows_are_readable() {
        let root = tempdir("concurrent");
        let j = std::sync::Arc::new(journal(&root.join("journal.db")));
        let mut workers = Vec::new();
        for n in 0..8 {
            let j = j.clone();
            workers.push(thread::spawn(move || {
                let (server, mut client) = UnixStream::pair().unwrap();
                let server_thread = thread::spawn(move || {
                    serve_one(server, &j, rustix::process::getuid().as_raw()).unwrap()
                });
                let mut payload = serde_json::Map::new();
                payload.insert("command".into(), serde_json::json!(format!("echo {n}")));
                let event = li_proto::Event::new(
                    li_proto::EventId::new(format!("daemon-test-{n}")).unwrap(),
                    li_proto::Source::ShellBash,
                    None,
                    0,
                    n,
                    "2026-11-24T00:00:00.000Z",
                    li_proto::EventType::new("CommandFinished").unwrap(),
                    payload,
                    li_proto::EvidenceStatus::Declared,
                )
                .unwrap();
                let mut line = event.to_json_line().unwrap();
                line.push('\n');
                client.write_all(line.as_bytes()).unwrap();
                drop(client);
                server_thread.join().unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let water = j.flush().unwrap();
        assert_eq!(
            water.failed,
            0,
            "watermark={water:?}, error={:?}",
            j.last_error()
        );
        assert_eq!(j.reader().unwrap().count("events").unwrap(), 8);
        drop(j);
        let _ = fs::remove_dir_all(root);
    }
}
