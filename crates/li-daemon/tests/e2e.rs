use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use li_daemon::server::{bind, record_startup, run_accept_loop, socket_path};
use li_daemon::state::DaemonState;
use li_hook::{Adapter, Queue, Sink, ingest};
use li_journal::{Config, Journal};
use li_proto::{
    Ping, SchemaVersion, ServerMessage, SessionId, decode_server_message, encode_frame,
};
use tokio::sync::watch;

fn tempdir(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("li-e2e-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_hook_to_daemon_to_journal_and_restart_reconciliation() {
    let root = tempdir("full");
    let runtime_dir = root.join("run");
    let journal_path = root.join("state").join("journal.db");

    let mut journal = Journal::open(&journal_path, Config::default()).unwrap();
    journal.migrate().unwrap();
    let known = record_startup(&journal, "daemon-e2e-1", "boot-e2e").unwrap();
    let journal = Arc::new(journal);
    let state = Arc::new(DaemonState::new(Arc::clone(&journal), "boot-e2e".into()));
    state.seed_known_sessions(known);

    let listener = bind(&runtime_dir).unwrap();
    let sock = socket_path(&runtime_dir);
    let (tx, rx) = watch::channel(false);
    let owner = rustix::process::getuid().as_raw();
    let server_task = tokio::spawn(run_accept_loop(listener, Arc::clone(&state), owner, rx));

    // Drive `li_hook::ingest` over a real AF_UNIX socket on a blocking thread.
    let sock_for_hook = sock.clone();
    let hook_report = tokio::task::spawn_blocking(move || {
        let mut sink = Sink::connect(&sock_for_hook).unwrap();
        let cap = fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("capabilities/bash.json"),
        )
        .unwrap();
        let reqs = format!(
            "{{\"type\":\"SessionStarted\",\"ts_wall\":\"2026-11-24T12:00:00.000Z\",\"payload\":{{\"shell\":\"bash\",\"shell_version\":\"5.3.0\",\"pid\":1001,\"cwd\":\"/tmp\",\"capability_record\":{}}}}}\n\
             {{\"type\":\"CommandStarted\",\"ts_wall\":\"2026-11-24T12:00:01.000Z\",\"payload\":{{\"command\":\"echo hi\"}}}}\n\
             {{\"type\":\"CommandFinished\",\"ts_wall\":\"2026-11-24T12:00:02.000Z\",\"payload\":{{\"command\":\"echo hi\",\"exit_status\":0,\"path\":\"/tmp/e2e.txt\"}}}}\n\
             {{\"type\":\"SessionEnded\",\"ts_wall\":\"2026-11-24T12:00:03.000Z\",\"payload\":{{\"reason\":\"exit\"}}}}\n",
            cap.trim()
        );
        let sid = SessionId::new("e2e-bash-1").unwrap();
        ingest(
            std::io::Cursor::new(reqs),
            &mut sink,
            Queue::new(16),
            Adapter::Bash,
            &sid,
        )
    })
    .await
    .unwrap();

    assert_eq!(hook_report.accepted, 4);
    assert_eq!(hook_report.delivered, 4);
    assert_eq!(hook_report.dropped, 0);

    // Send a Ping barrier over a fresh client socket to verify flush + Pong watermark.
    let sock_for_ping = sock.clone();
    let pong = tokio::task::spawn_blocking(move || {
        let mut client = StdUnixStream::connect(&sock_for_ping).unwrap();
        let mut ping = encode_frame(&Ping {
            schema_version: SchemaVersion::CURRENT,
            nonce: 777,
        })
        .unwrap();
        ping.push('\n');
        client.write_all(ping.as_bytes()).unwrap();
        let mut resp = String::new();
        std::io::BufReader::new(&mut client)
            .read_line(&mut resp)
            .unwrap();
        decode_server_message(resp.trim()).unwrap()
    })
    .await
    .unwrap();

    use std::io::BufRead as _;
    let ServerMessage::Pong(pong) = pong else {
        panic!("expected Pong");
    };
    assert_eq!(pong.nonce, 777);
    assert_eq!(pong.counts.ingested, 4);
    assert_eq!(pong.watermarks[0].contiguous_through, Some(3));
    assert_eq!(pong.watermarks[0].highest_seen, Some(3));

    let _ = tx.send(true);
    server_task.await.unwrap().unwrap();

    // Verify SQLite persisted state & FK integrity.
    let r = journal.reader().unwrap();
    assert_eq!(r.foreign_key_violations().unwrap(), 0);
    assert_eq!(r.integrity_check().unwrap(), "ok");
    assert_eq!(r.count("adapter_registry").unwrap(), 1);
    let sessions = r.list_sessions(10).unwrap();
    let bash_sess = sessions
        .iter()
        .find(|s| s.session_id == "e2e-bash-1")
        .unwrap();
    assert!(bash_sess.ended_wall.is_some());
    let path_hits = r.events_touching_path("/tmp/e2e.txt", 10).unwrap();
    assert_eq!(path_hits.len(), 1);
    let stored = r.query_events(Some("e2e-bash-1"), None, None, 10).unwrap();
    assert_eq!(stored.len(), 4);
    assert!(stored.iter().all(|e| e.ts_mono_ns > 0));

    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_thousand_events_burst_has_zero_unexplained_gaps() {
    use std::io::BufRead as _;

    let root = tempdir("10k-burst");
    let runtime_dir = root.join("run");
    let journal_path = root.join("state").join("journal.db");

    let mut journal = Journal::open(&journal_path, Config::default()).unwrap();
    journal.migrate().unwrap();
    let known = record_startup(&journal, "daemon-10k-1", "boot-10k").unwrap();
    let journal = Arc::new(journal);
    let state = Arc::new(DaemonState::new(Arc::clone(&journal), "boot-10k".into()));
    state.seed_known_sessions(known);

    let listener = bind(&runtime_dir).unwrap();
    let sock = socket_path(&runtime_dir);
    let (tx, rx) = watch::channel(false);
    let owner = rustix::process::getuid().as_raw();
    let server_task = tokio::spawn(run_accept_loop(listener, Arc::clone(&state), owner, rx));

    // Send 10,000 events across 20 paced batches of 500 on one persistent socket,
    // followed by a same-connection Ping barrier (spec §2 / A5).
    let sock_for_client = sock.clone();
    let pong = tokio::task::spawn_blocking(move || {
        let mut client = StdUnixStream::connect(&sock_for_client).unwrap();
        let _ = client.set_write_timeout(Some(std::time::Duration::from_secs(5)));
        let _ = client.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        let sid = SessionId::new("burst-10k").unwrap();
        for i in 0..10_000u64 {
            let mut payload = serde_json::Map::new();
            payload.insert("command".into(), serde_json::json!(format!("echo {i}")));
            payload.insert("exit_status".into(), serde_json::json!(0));
            let ev = li_proto::Event::new(
                li_proto::EventId::new(format!("burst-10k-{i}")).unwrap(),
                li_proto::Source::ShellBash,
                Some(sid.clone()),
                i,
                0,
                "2026-11-27T06:00:00.000Z",
                li_proto::EventType::new("CommandFinished").unwrap(),
                payload,
                li_proto::EvidenceStatus::Declared,
            )
            .unwrap();
            let mut line = ev.to_json_line().unwrap();
            line.push('\n');
            client.write_all(line.as_bytes()).unwrap();
            if i % 500 == 499 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        let mut ping = encode_frame(&Ping {
            schema_version: SchemaVersion::CURRENT,
            nonce: 10_000,
        })
        .unwrap();
        ping.push('\n');
        client.write_all(ping.as_bytes()).unwrap();
        let mut resp = String::new();
        std::io::BufReader::new(&mut client)
            .read_line(&mut resp)
            .unwrap();
        decode_server_message(resp.trim()).unwrap()
    })
    .await
    .unwrap();

    let ServerMessage::Pong(pong) = pong else {
        panic!("expected Pong");
    };
    assert_eq!(pong.counts.ingested, 10_000);
    assert_eq!(pong.watermarks[0].contiguous_through, Some(9_999));
    assert_eq!(pong.watermarks[0].highest_seen, Some(9_999));

    let _ = tx.send(true);
    server_task.await.unwrap().unwrap();

    record_startup(&journal, "daemon-10k-2", "boot-10k").unwrap();
    let r = journal.reader().unwrap();
    assert_eq!(r.foreign_key_violations().unwrap(), 0);
    assert_eq!(r.integrity_check().unwrap(), "ok");
    let rows = r.events_of_session("burst-10k", 20_000).unwrap();
    assert_eq!(rows.len(), 10_000);
    assert_eq!(r.sequence_gaps("burst-10k").unwrap().len(), 0);
    let restarts = r
        .query_events(None, None, Some("DaemonRestarted"), 5)
        .unwrap();
    assert_eq!(restarts.len(), 1);
    assert_eq!(restarts[0].payload["unexplained_gaps"], 0);

    let _ = fs::remove_dir_all(root);
}
