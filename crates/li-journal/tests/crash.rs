//! Task item 2: a **real** `SIGKILL` mid-write, then reopen and check.
//!
//! Nothing here is simulated. The child is a separate process
//! (`li-journal-crash-probe`), it is killed with signal 9, and the assertions are
//! on the resulting file: `PRAGMA integrity_check`, the committed prefix, and the
//! absence of a half-applied transaction.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use common::fresh_dir;
use li_journal::db::Config;
use li_journal::{Journal, Reader};

/// Sends `SIGKILL` (signal 9) to a pid. `kill(1)` rather than `libc::kill`: this
/// crate forbids `unsafe`, and a shelling out to `kill` is honest about what is
/// being tested.
fn sigkill(pid: u32) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("invoke kill");
    assert!(status.success(), "kill -9 failed");
}

#[test]
fn kill_9_leaves_a_consistent_database() {
    let dir = fresh_dir("t-kill9");
    let db = dir.join("journal.db");

    let probe = env!("CARGO_BIN_EXE_li-journal-crash-probe");
    let mut child = Command::new(probe)
        .arg(&db)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn crash probe");
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();

    // Wait until the child has actually committed something, so the kill lands
    // during real work rather than during startup. 5 reports = 160 rows.
    let mut last_reported = 0u64;
    let mut last_id = String::new();
    for _ in 0..5 {
        let line = lines
            .next()
            .expect("probe must report progress")
            .expect("read line");
        let parts: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(parts[0], "committed", "unexpected probe output: {line}");
        last_reported = parts[1].parse().expect("written count");
        last_id = parts[2].to_string();
    }
    assert!(
        last_reported >= 160,
        "expected real progress, got {last_reported}"
    );

    // The kill. No shutdown, no flush, no chance to clean up.
    sigkill(pid);
    let status = child.wait().expect("reap");
    assert!(
        !status.success(),
        "the child must have died from the signal, not exited normally"
    );

    // Reopen exactly as a restarted daemon would.
    let reader = Reader::open(&db, &Config::default()).expect("reopen after SIGKILL");
    assert_eq!(
        reader.integrity_check().expect("integrity_check"),
        "ok",
        "spec §6 criterion 4"
    );
    assert_eq!(
        reader.foreign_key_violations().expect("fk check"),
        0,
        "a torn transaction must not survive as an orphan"
    );

    // Everything the child reported as committed must still be there.
    let events = reader.count("events").expect("count");
    assert!(
        events >= last_reported as i64,
        "committed rows were lost: reported {last_reported}, found {events}"
    );
    assert!(
        reader.payload_of(&last_id).expect("read payload").is_some(),
        "the last reported committed event {last_id} is gone"
    );

    // The committed prefix is contiguous: `sequence` runs 0..n with no hole, which
    // is what "no half-applied transaction" means for this schema.
    let gaps = reader.sequence_gaps("crash").expect("gaps");
    assert!(
        gaps.is_empty(),
        "the killed writer left sequence holes: {gaps:?}"
    );
    let max_seq: i64 = reader
        .connection()
        .query_row("SELECT max(sequence) FROM events", [], |r| r.get(0))
        .expect("max sequence");
    assert_eq!(
        max_seq + 1,
        events,
        "rows must form a contiguous prefix: max sequence {max_seq}, {events} rows"
    );

    // And the file is still writable afterwards: recovery must not leave a lock.
    drop(reader);
    let mut journal = Journal::open(&db, Config::default()).expect("open for writing");
    let applied = journal.migrate().expect("migrate after a crash");
    assert!(
        applied.is_empty(),
        "the schema was already current; re-applying {applied:?} would mean the ledger was lost"
    );
    journal
        .event(&probe_event("after-crash"))
        .expect("submit after crash");
    let w = journal.flush().expect("flush after crash");
    assert_eq!(w.failed, 0, "post-crash write must succeed");
    let reader = journal.reader().unwrap();
    assert!(reader.payload_of("after-crash").unwrap().is_some());
    assert_eq!(reader.integrity_check().unwrap(), "ok");
}

fn probe_event(id: &str) -> li_proto::Event {
    let mut payload = serde_json::Map::new();
    payload.insert("command".into(), serde_json::json!("echo after"));
    li_proto::Event::new(
        li_proto::EventId::new(id).unwrap(),
        li_proto::Source::ShellBash,
        Some(li_proto::SessionId::new("crash").unwrap()),
        // `sequence` must be 0 when the session is absent; here it is present, so
        // any value is legal. Reuse a high one so it cannot collide with the
        // killed writer's prefix.
        1_000_000,
        1,
        "2026-11-24T00:00:00.000Z",
        li_proto::EventType::new("CommandFinished").unwrap(),
        payload,
        li_proto::EvidenceStatus::Declared,
    )
    .unwrap()
}
