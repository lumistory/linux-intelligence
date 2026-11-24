//! Shared helpers. Each test gets its own directory so no test can pass because
//! of a file another test left behind.

// Each test binary includes this module and uses a different subset of it, so
// "unused in this binary" is normal here and not a defect.
#![allow(dead_code)]

use std::path::PathBuf;

use li_journal::db::Config;
use li_journal::{Journal, write};

/// A fresh, empty directory. `unique` must be distinct per test.
pub fn fresh_dir(unique: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("li-journal-tests")
        .join(format!("{unique}-{}", std::process::id()));
    // A leftover from an interrupted run would make "already migrated" look like
    // "migration applied", so remove this test's own files only.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

pub fn open_migrated(dir: &std::path::Path, config: Config) -> Journal {
    let mut j = Journal::open(&dir.join("journal.db"), config).expect("open journal");
    j.migrate().expect("migrate");
    j
}

pub fn session(id: &str) -> write::Write {
    write::Write::Session(write::Session {
        session_id: id.to_string(),
        parent_session: None,
        adapter: "bash".to_string(),
        adapter_version: None,
        shell_pid: None,
        started_wall: "2026-11-24T00:00:00.000Z".to_string(),
        started_mono_ns: 1,
        cwd: None,
        host: None,
        boot_id: None,
    })
}

/// A protocol event with a payload, shaped like something a hook would send.
pub fn event(
    id: &str,
    session: &str,
    sequence: u64,
    type_: &str,
    payload: serde_json::Value,
) -> li_proto::Event {
    let payload = match payload {
        serde_json::Value::Object(m) => m,
        other => panic!("payload must be an object, got {other}"),
    };
    li_proto::Event::new(
        li_proto::EventId::new(id).expect("event id"),
        li_proto::Source::ShellBash,
        Some(li_proto::SessionId::new(session).expect("session id")),
        sequence,
        1_000_000 * sequence + 1,
        "2026-11-24T00:00:00.000Z",
        li_proto::EventType::new(type_).expect("event type"),
        payload,
        li_proto::EvidenceStatus::Declared,
    )
    .expect("event")
}
