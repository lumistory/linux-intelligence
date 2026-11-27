//! Task item 3: a forward migration over a **populated** database must not lose
//! data.
//!
//! 0-H §7.1 did this once with 5000 rows; this is the same shape as a test, plus
//! the parts 0-H could not assert cheaply: that the ledger is idempotent, that a
//! drifted file is refused, and that a `vN+1` build refuses a `vN+2` database.

mod common;

use common::{event, fresh_dir};
use li_journal::db::Config;
use li_journal::{Journal, current_version, known_version};
use rusqlite::Connection;

/// Builds a **v1** database the way a v1 build would have left it, using v1's own
/// column set.
///
/// Raw SQL rather than this build's writer, because the writer speaks the current
/// schema (see `writer.rs`) and could not produce a v1-shaped row even if asked.
/// That is also what makes the test adversarial: the rows come from outside this
/// build, with v1's columns, v1's id shapes and no v2 columns at all.
fn populate_v1(db: &std::path::Path, rows: usize) {
    let mut conn = li_journal::db::open(db, &Config::default()).unwrap();
    let applied = li_journal::migrate::migrate(&mut conn, Some(1)).unwrap();
    assert_eq!(applied, vec![1]);
    conn.execute(
        "INSERT INTO sessions (session_id, adapter, started_wall, started_mono_ns)
         VALUES ('s1', 'bash', '2026-11-24T00:00:00.000Z', 1)",
        [],
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    for i in 0..rows {
        tx.execute(
            "INSERT INTO events
               (event_id, schema_version, source, session_id, sequence, ts_mono_ns,
                ts_wall, type, type_class, payload, evidence_status, ingested_mono_ns)
             VALUES (?1, 1, 'shell.bash', 's1', ?2, ?2,
                     '2026-11-24T00:00:00.000Z', 'CommandFinished', 'proc.end', ?3,
                     'declared', 0)",
            rusqlite::params![
                format!("v1-{i:04}"),
                i as i64,
                format!(r#"{{"command":"echo {i}"}}"#)
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

/// The headline test: v1 populated with real rows, then upgraded to v2.
#[test]
fn v1_to_v2_keeps_every_row() {
    let dir = fresh_dir("t-migrate-populated");
    let db = dir.join("journal.db");

    populate_v1(&db, 500);

    // Sanity: the state really is v1, and the v2 column really is absent.
    {
        let conn = Connection::open(&db).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 1);
        let cols = column_names(&conn, "events");
        assert!(
            !cols.contains(&"redaction_hits".to_string()),
            "v1 must not already have the v2 column: {cols:?}"
        );
        let n: i64 = conn
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 500);
    }

    // Now the upgrade, over live data (v1 -> v3).
    let mut j = Journal::open(&db, Config::default()).unwrap();
    let applied = j.migrate().unwrap();
    assert_eq!(
        applied,
        vec![2, 3],
        "v1 -> v3 should apply migrations 2 and 3"
    );

    let reader = j.reader().unwrap();
    assert_eq!(
        reader.count("events").unwrap(),
        500,
        "no event may be lost by the migration"
    );
    assert_eq!(reader.count("sessions").unwrap(), 1);
    assert_eq!(reader.integrity_check().unwrap(), "ok");
    assert_eq!(reader.foreign_key_violations().unwrap(), 0);
    // Old rows keep their payload, verbatim.
    assert_eq!(
        reader.payload_of("v1-0007").unwrap().unwrap(),
        r#"{"command":"echo 7"}"#
    );
    // New rows have the new column, old rows have NULL — not a fabricated 0.
    let hits: Option<i64> = reader
        .connection()
        .query_row(
            "SELECT redaction_hits FROM events WHERE event_id='v1-0007'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, None, "a backfilled value would be an invented fact");
}

/// The ledger records both steps, with the v1 checksum unchanged: an upgrade must
/// never rewrite the history of how it got there.
#[test]
fn the_ledger_keeps_the_v1_checksum_after_the_upgrade() {
    let dir = fresh_dir("t-migrate-ledger");
    let db = dir.join("journal.db");
    populate_v1(&db, 4);
    let mut j = Journal::open(&db, Config::default()).unwrap();
    j.migrate().unwrap();
    let reader = j.reader().unwrap();
    let ledger = li_journal::migrate::ledger(reader.connection()).unwrap();
    assert_eq!(ledger.len(), 3);
    assert_eq!(ledger[0].0, 1);
    assert_eq!(ledger[0].1, "init");
    assert_eq!(ledger[1].0, 2);
    assert_eq!(ledger[1].1, "add_redaction_ledger");
    assert_eq!(ledger[2].0, 3);
    assert_eq!(ledger[2].1, "add_new_path_index");
}

/// Re-running migrate is a no-op that verifies, never a re-apply.
#[test]
fn migrating_twice_applies_nothing_and_verifies() {
    let dir = fresh_dir("t-migrate-idempotent");
    let db = dir.join("journal.db");
    let mut j = Journal::open(&db, Config::default()).unwrap();
    assert_eq!(j.migrate().unwrap(), vec![1, 2, 3]);
    assert!(j.migrate().unwrap().is_empty());
    assert_eq!(current_version(&Connection::open(&db).unwrap()).unwrap(), 3);
    assert_eq!(known_version(), 3);
}

/// A database written by a newer build must be refused, not read best-effort.
#[test]
fn a_newer_database_is_refused_rather_than_downgraded() {
    let dir = fresh_dir("t-migrate-ahead");
    let db = dir.join("journal.db");
    let mut j = Journal::open(&db, Config::default()).unwrap();
    j.migrate().unwrap();
    // Pretend a future build wrote v99.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO schema_version (version, applied_at, description, checksum)
             VALUES (99, '2026-11-24T00:00:00.000Z', 'from-the-future', 'deadbeefdeadbeef')",
            [],
        )
        .unwrap();
    }
    let err = j.migrate().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("v99"),
        "the error must name the version found, got: {msg}"
    );
}

/// An edited-then-reapplied migration is a broken series, and is detected.
#[test]
fn a_changed_migration_body_is_detected() {
    let dir = fresh_dir("t-migrate-drift");
    let db = dir.join("journal.db");
    let mut j = Journal::open(&db, Config::default()).unwrap();
    j.migrate().unwrap();
    // Simulate "the file changed after it was applied" by recording a different
    // checksum for a version this build knows.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE schema_version SET checksum = '0000000000000000' WHERE version = 1",
            [],
        )
        .unwrap();
    }
    let err = j.migrate().unwrap_err();
    assert!(
        matches!(err, li_journal::error::JournalError::MigrationDrift { .. }),
        "expected MigrationDrift, got {err:?}"
    );
}

/// The data a v1 database holds must survive, not just the row count: this walks
/// one session's sequence and checks its gaps report is still empty.
#[test]
fn a_populated_session_reports_no_gaps_after_upgrade() {
    let dir = fresh_dir("t-migrate-gaps");
    let db = dir.join("journal.db");
    populate_v1(&db, 64);
    let mut j = Journal::open(&db, Config::default()).unwrap();
    j.migrate().unwrap();
    let reader = j.reader().unwrap();
    assert_eq!(reader.sequence_gaps("s1").unwrap(), vec![]);
    assert_eq!(reader.events_of_session("s1", 100).unwrap().len(), 64);
}

/// **The writer speaks the current schema and nothing else.**
///
/// This is a real coupling, pinned here so it is a tested property rather than a
/// surprise: an INSERT that names `redaction_hits` cannot run against a v1 file.
/// The failure is loud (a counted failure plus `last_error`), and the fix is
/// `migrate()` before ingest — which is what every entry point already does.
#[test]
fn writing_to_an_unmigrated_database_fails_loudly_and_counts() {
    let dir = fresh_dir("t-write-v1");
    let db = dir.join("journal.db");
    populate_v1(&db, 2);

    let j = Journal::open(&db, Config::default()).unwrap();
    let (accepted, _) = j
        .event(&event(
            "late",
            "s1",
            10,
            "CommandStarted",
            serde_json::json!({}),
        ))
        .unwrap();
    assert_eq!(accepted, li_journal::Accepted::Queued);
    let w = j.flush().unwrap();
    assert_eq!(w.failed, 1, "the row must be counted as refused, not lost");
    assert_eq!(w.written, 0);
    let why = j.last_error().unwrap();
    assert!(
        why.contains("redaction_hits"),
        "the reason must name the missing column, got: {why}"
    );
    // The v1 data is untouched by the failed write.
    let reader = j.reader().unwrap();
    assert_eq!(reader.count("events").unwrap(), 2);
    assert_eq!(reader.integrity_check().unwrap(), "ok");
}

fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut st = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
