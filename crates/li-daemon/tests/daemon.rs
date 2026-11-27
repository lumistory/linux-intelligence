use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use li_daemon::server::{ServiceError, bind, record_startup, serve_connection, socket_path};
use li_daemon::state::DaemonState;
use li_journal::{Config, Journal};
use li_proto::{
    DropReport, ErrorCode, Event, EventId, EventType, EvidenceStatus, Ping, SchemaVersion,
    ServerMessage, SessionId, Source, decode_server_message, encode_frame,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn tempdir(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("li-daemon-tokio-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&p).unwrap();
    p
}

fn open_journal(path: &Path) -> Journal {
    let mut j = Journal::open(path, Config::default()).unwrap();
    j.migrate().unwrap();
    j
}

#[tokio::test]
async fn socket_and_directory_permissions_are_private() {
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

#[tokio::test]
async fn peer_uid_mismatch_is_rejected_with_error_frame() {
    let root = tempdir("uid");
    let j = Arc::new(open_journal(&root.join("journal.db")));
    let st = Arc::new(DaemonState::new(j, "boot-a".into()));
    let (server, mut client) = UnixStream::pair().unwrap();
    let res = serve_connection(server, st, u32::MAX, None).await;
    assert!(matches!(res, Err(ServiceError::Unauthorized)));

    let mut line = String::new();
    BufReader::new(&mut client)
        .read_line(&mut line)
        .await
        .unwrap();
    let msg = decode_server_message(line.trim()).unwrap();
    assert!(matches!(
        msg,
        ServerMessage::Error(e) if e.code == ErrorCode::PeerUidRejected
    ));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn session_started_registers_fk_session_adapter_clock_and_real_pong_watermark() {
    let root = tempdir("session-fk");
    let j = Arc::new(open_journal(&root.join("journal.db")));
    let st = Arc::new(DaemonState::new(Arc::clone(&j), "boot-a".into()));
    let (server, client) = UnixStream::pair().unwrap();
    let uid = rustix::process::getuid().as_raw();
    let task = tokio::spawn(async move { serve_connection(server, st, uid, None).await });

    let (r, mut w) = client.into_split();
    let mut reader = BufReader::new(r);

    let mut cap_payload = serde_json::Map::new();
    cap_payload.insert("shell".into(), serde_json::json!("bash"));
    cap_payload.insert("shell_version".into(), serde_json::json!("5.3.0"));
    cap_payload.insert("pid".into(), serde_json::json!(4242));
    cap_payload.insert("cwd".into(), serde_json::json!("/tmp"));
    cap_payload.insert(
        "capability_record".into(),
        serde_json::json!({"dead_sink": true}),
    );

    let sid = SessionId::new("bash-live-1").unwrap();
    let ev0 = Event::new(
        EventId::new("bash-live-1-0").unwrap(),
        Source::ShellBash,
        Some(sid.clone()),
        0,
        0, // ts_mono_ns == 0 per N11; daemon must stamp it
        "2026-11-24T00:00:00.000Z",
        EventType::new("SessionStarted").unwrap(),
        cap_payload,
        EvidenceStatus::Declared,
    )
    .unwrap();
    // Skip seq 1 (client drop) -> send seq 2.
    let ev2 = Event::new(
        EventId::new("bash-live-1-2").unwrap(),
        Source::ShellBash,
        Some(sid.clone()),
        2,
        0,
        "2026-11-24T00:00:01.000Z",
        EventType::new("CommandFinished").unwrap(),
        serde_json::Map::new(),
        EvidenceStatus::Declared,
    )
    .unwrap();

    for ev in [ev0, ev2] {
        let mut line = ev.to_json_line().unwrap();
        line.push('\n');
        w.write_all(line.as_bytes()).await.unwrap();
    }

    let ping = Ping {
        schema_version: SchemaVersion::CURRENT,
        nonce: 99,
    };
    let mut ping_line = encode_frame(&ping).unwrap();
    ping_line.push('\n');
    w.write_all(ping_line.as_bytes()).await.unwrap();

    let mut resp = String::new();
    reader.read_line(&mut resp).await.unwrap();
    let ServerMessage::Pong(pong) = decode_server_message(resp.trim()).unwrap() else {
        panic!("expected pong, got {resp}");
    };
    assert_eq!(pong.nonce, 99);
    assert_eq!(pong.counts.ingested, 2);
    assert_eq!(pong.counts.seq_violations, 1);
    assert_eq!(pong.watermarks.len(), 1);
    assert_eq!(pong.watermarks[0].session_id, sid);
    assert_eq!(pong.watermarks[0].contiguous_through, Some(0));
    assert_eq!(pong.watermarks[0].highest_seen, Some(2));

    drop(w);
    task.await.unwrap().unwrap();

    let r = j.reader().unwrap();
    assert_eq!(r.foreign_key_violations().unwrap(), 0);
    assert_eq!(r.count("sessions").unwrap(), 1);
    assert_eq!(r.count("adapter_registry").unwrap(), 1);
    let stored = r.query_events(Some("bash-live-1"), None, None, 10).unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(|e| e.ts_mono_ns > 0));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn overlong_frame_returns_error_and_connection_survives() {
    let root = tempdir("overlong");
    let j = Arc::new(open_journal(&root.join("journal.db")));
    let st = Arc::new(DaemonState::new(j, "boot-a".into()));
    let (server, client) = UnixStream::pair().unwrap();
    let uid = rustix::process::getuid().as_raw();
    let task = tokio::spawn(async move { serve_connection(server, st, uid, None).await });

    let (r, mut w) = client.into_split();
    let mut reader = BufReader::new(r);

    let bad = format!("{}\n", "x".repeat(li_proto::MAX_FRAME_BYTES + 16));
    w.write_all(bad.as_bytes()).await.unwrap();

    let mut err_line = String::new();
    reader.read_line(&mut err_line).await.unwrap();
    let ServerMessage::Error(err) = decode_server_message(err_line.trim()).unwrap() else {
        panic!("expected error frame");
    };
    assert_eq!(err.code, ErrorCode::PayloadTooLarge);

    // Connection must still accept a Ping afterwards.
    let mut ping_line = encode_frame(&Ping {
        schema_version: SchemaVersion::CURRENT,
        nonce: 7,
    })
    .unwrap();
    ping_line.push('\n');
    w.write_all(ping_line.as_bytes()).await.unwrap();

    let mut pong_line = String::new();
    reader.read_line(&mut pong_line).await.unwrap();
    assert!(matches!(
        decode_server_message(pong_line.trim()).unwrap(),
        ServerMessage::Pong(p) if p.nonce == 7
    ));

    drop(w);
    let res = task.await.unwrap();
    assert!(res.is_ok(), "unexpected error: {res:?}");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn partial_frame_without_newline_times_out_instead_of_stalling() {
    let root = tempdir("partial-timeout");
    let j = Arc::new(open_journal(&root.join("journal.db")));
    let st = Arc::new(DaemonState::new(j, "boot-a".into()));
    let (server, mut client) = UnixStream::pair().unwrap();
    let uid = rustix::process::getuid().as_raw();
    let task = tokio::spawn(async move { serve_connection(server, st, uid, None).await });

    client
        .write_all(br#"{"partial":"no_newline"#)
        .await
        .unwrap();
    let res = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("handler must time out partial frame within 5s")
        .unwrap();
    assert!(res.is_ok());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn drop_report_explains_tail_and_startup_reconciliation() {
    let root = tempdir("drop-report");
    let j = Arc::new(open_journal(&root.join("journal.db")));
    let st = Arc::new(DaemonState::new(Arc::clone(&j), "boot-a".into()));
    record_startup(&j, "daemon-1", "boot-a").unwrap();

    let (server, mut client) = UnixStream::pair().unwrap();
    let uid = rustix::process::getuid().as_raw();
    let task = tokio::spawn(async move { serve_connection(server, st, uid, None).await });

    let sid = SessionId::new("sess-gap").unwrap();
    for seq in [0u64, 2u64] {
        let ev = Event::new(
            EventId::new(format!("sess-gap-{seq}")).unwrap(),
            Source::ShellBash,
            Some(sid.clone()),
            seq,
            100 + seq,
            "2026-11-24T00:00:00.000Z",
            EventType::new("CommandFinished").unwrap(),
            serde_json::Map::new(),
            EvidenceStatus::Declared,
        )
        .unwrap();
        let mut line = ev.to_json_line().unwrap();
        line.push('\n');
        client.write_all(line.as_bytes()).await.unwrap();
    }
    let dr = DropReport {
        schema_version: SchemaVersion::CURRENT,
        session_id: sid,
        dropped: 1,
        last_sequence: Some(2),
        reason: "queue_full".into(),
    };
    let mut dr_line = encode_frame(&dr).unwrap();
    dr_line.push('\n');
    client.write_all(dr_line.as_bytes()).await.unwrap();

    drop(client);
    task.await.unwrap().unwrap();
    j.flush().unwrap();

    record_startup(&j, "daemon-2", "boot-a").unwrap();
    let r = j.reader().unwrap();
    let events = r
        .query_events(None, None, Some("DaemonRestarted"), 5)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["observed_gap_count"], 1);
    assert_eq!(events[0].payload["reported_client_drops"], 1);
    assert_eq!(events[0].payload["unexplained_gaps"], 0);
    assert_eq!(
        events[0].payload["producer_tail_status"],
        "client_drop_reported"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_1000_clients_under_semaphore_keeps_daemon_healthy() {
    use li_daemon::server::{MAX_CONCURRENT_CLIENTS, run_accept_loop};
    use tokio::sync::watch;
    use tokio::task::JoinSet;

    let root = tempdir("1000-clients");
    let listener = bind(&root).unwrap();
    let sock = socket_path(&root);
    let j = Arc::new(open_journal(&root.join("journal.db")));
    let st = Arc::new(DaemonState::new(Arc::clone(&j), "boot-1k".into()));
    let uid = rustix::process::getuid().as_raw();
    let (tx, rx) = watch::channel(false);
    let srv = tokio::spawn(run_accept_loop(listener, st, uid, rx));

    let mut set = JoinSet::new();
    for i in 0..1_000_u64 {
        let p = sock.clone();
        set.spawn(async move {
            let Ok(mut stream) = UnixStream::connect(&p).await else {
                return false;
            };
            let ping = Ping {
                schema_version: SchemaVersion::CURRENT,
                nonce: i,
            };
            let Ok(mut line) = encode_frame(&ping) else {
                return false;
            };
            line.push('\n');
            if stream.write_all(line.as_bytes()).await.is_err() {
                return false;
            }
            let mut resp = String::new();
            let mut br = BufReader::new(stream);
            match br.read_line(&mut resp).await {
                Ok(n) if n > 0 => matches!(
                    decode_server_message(&resp),
                    Ok(ServerMessage::Pong(p)) if p.nonce == i
                ),
                _ => false,
            }
        });
    }

    let mut succeeded = 0usize;
    while let Some(res) = set.join_next().await {
        if res.unwrap_or(false) {
            succeeded += 1;
        }
    }
    assert!(
        succeeded >= MAX_CONCURRENT_CLIENTS,
        "expected >= {MAX_CONCURRENT_CLIENTS} pongs during 1000-client burst, got {succeeded}"
    );

    // Post-burst health check: daemon still serves cleanly after excess connects were dropped.
    let mut after = UnixStream::connect(&sock).await.unwrap();
    let mut ping_line = encode_frame(&Ping {
        schema_version: SchemaVersion::CURRENT,
        nonce: 9_999,
    })
    .unwrap();
    ping_line.push('\n');
    after.write_all(ping_line.as_bytes()).await.unwrap();
    let mut after_resp = String::new();
    let n = BufReader::new(after)
        .read_line(&mut after_resp)
        .await
        .unwrap();
    assert!(n > 0);
    assert!(matches!(
        decode_server_message(&after_resp),
        Ok(ServerMessage::Pong(p)) if p.nonce == 9_999
    ));

    let _ = tx.send(true);
    srv.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(root);
}
