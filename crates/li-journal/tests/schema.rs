//! Schema gates: the append-only triggers, the JSON index, and the constraints
//! that make a lie unrepresentable.

mod common;

use common::{event, fresh_dir, open_migrated};
use li_journal::db::Config;
use rusqlite::Connection;

/// Task item 1, half A: `UPDATE events` must be refused **by the database**, with
/// a specific message. Asserting only `is_err()` would pass on a typo in the SQL.
#[test]
fn update_on_events_is_refused_by_a_trigger() {
    let dir = fresh_dir("t-no-update");
    let j = open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    j.submit(li_journal::write::Write::Event(
        li_journal::write::EventRow {
            event_id: "e1".into(),
            schema_version: 1,
            source: "shell.bash".into(),
            session_id: Some("s1".into()),
            process_row_id: None,
            sequence: 0,
            ts_mono_ns: 1,
            ts_wall: "2026-11-24T00:00:00.000Z".into(),
            type_: "CommandFinished".into(),
            type_class: "proc.end".into(),
            payload: "{}".into(),
            evidence_status: "declared".into(),
            dedup_group: None,
            duplicate_of: None,
            redaction_rules: None,
            redaction_hits: None,
            redaction_digest: None,
            fp_type_class: None,
            fp_subject: None,
            fp_bucket_ns: None,
            ingested_mono_ns: 0,
        },
    ));
    j.flush().unwrap();

    // A *writing* connection, because this must be refused by the trigger and not
    // by the read-only open flags — those would be a different guarantee.
    let conn = Connection::open(j.path()).unwrap();
    let err = conn
        .execute(
            "UPDATE events SET type = 'FileDeleted' WHERE event_id = 'e1'",
            [],
        )
        .expect_err("UPDATE must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("append_only") && msg.contains("UPDATE"),
        "the error must name the reason, got: {msg}"
    );
    assert_eq!(
        conn.query_row("SELECT type FROM events WHERE event_id='e1'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "CommandFinished",
        "the refused UPDATE must not have changed the row"
    );
}

/// Task item 1, half B: `DELETE FROM events` — all rows — must be refused.
///
/// The statement-level form is the one that matters: a row-level trigger would
/// still abort, but a `DELETE` with no rows matching would *succeed* and look
/// harmless, so the assertion is on a table that definitely has rows.
#[test]
fn delete_from_events_is_refused_by_a_trigger() {
    let dir = fresh_dir("t-no-delete");
    let j = open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    j.submit(li_journal::write::Write::Event(
        li_journal::write::EventRow {
            event_id: "e1".into(),
            schema_version: 1,
            source: "shell.bash".into(),
            session_id: Some("s1".into()),
            process_row_id: None,
            sequence: 0,
            ts_mono_ns: 1,
            ts_wall: "2026-11-24T00:00:00.000Z".into(),
            type_: "CommandFinished".into(),
            type_class: "proc.end".into(),
            payload: "{}".into(),
            evidence_status: "declared".into(),
            dedup_group: None,
            duplicate_of: None,
            redaction_rules: None,
            redaction_hits: None,
            redaction_digest: None,
            fp_type_class: None,
            fp_subject: None,
            fp_bucket_ns: None,
            ingested_mono_ns: 0,
        },
    ));
    j.flush().unwrap();
    let reader = j.reader().unwrap();
    assert_eq!(reader.count("events").unwrap(), 1);

    let conn = Connection::open(j.path()).unwrap();
    let err = conn
        .execute("DELETE FROM events", [])
        .expect_err("DELETE must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("append_only") && msg.contains("DELETE"),
        "the error must name the reason, got: {msg}"
    );
    assert_eq!(
        reader.count("events").unwrap(),
        1,
        "the refused DELETE must not have removed anything"
    );
}

/// The exact boundary of the trigger guarantee, pinned because it is weaker than
/// "DELETE always raises" and pretending otherwise would be a false claim.
///
/// `BEFORE DELETE` here is a **row** trigger (SQLite reserves statement triggers
/// for `INSTEAD OF`). So it fires once per row that matches: a `DELETE` that
/// matches one row raises, a `DELETE` that matches none raises nothing and
/// reports success having done nothing. `UPDATE` behaves the same way.
///
/// Which is the guarantee that matters — **no row can be altered or removed** —
/// but it is not "the statement always fails", so the test says what was measured.
#[test]
fn the_delete_trigger_fires_per_matching_row() {
    let dir = fresh_dir("t-no-truncate");
    let j = open_migrated(&dir, Config::default());
    let conn = Connection::open(j.path()).unwrap();

    // Empty table: nothing matches, nothing to protect, no error. This is the
    // documented boundary, asserted rather than glossed over.
    assert!(
        conn.execute("DELETE FROM events", []).is_ok(),
        "a DELETE matching no row has nothing to refuse"
    );
    assert!(
        conn.execute("UPDATE events SET sequence = 9", []).is_ok(),
        "an UPDATE matching no row has nothing to refuse"
    );

    // With one row present, both statements are refused.
    conn.execute(
        "INSERT INTO events
           (event_id, schema_version, source, sequence, ts_mono_ns, ts_wall,
            type, type_class, payload, evidence_status, ingested_mono_ns)
         VALUES ('e1', 1, 'fs', 0, 0, '2026-11-24T00:00:00.000Z',
                 'FileModified', 'fs.modify', '{}', 'observed', 0)",
        [],
    )
    .unwrap();
    let del = conn
        .execute("DELETE FROM events", [])
        .unwrap_err()
        .to_string();
    assert!(del.contains("append_only"), "{del}");
    let upd = conn
        .execute("UPDATE events SET sequence = 9", [])
        .unwrap_err()
        .to_string();
    assert!(upd.contains("append_only"), "{upd}");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

/// Task item 4: the JSON expression index must actually be used.
///
/// This is the test that stops "the index exists" from being mistaken for "the
/// index works". A query written as `payload -> '$.path'` or with the JSON text
/// built by `json_object` would scan, and 0-H measured that scan at 2664 us p50
/// against 16.6 us with the index.
#[test]
fn the_json_path_query_uses_the_expression_index() {
    let dir = fresh_dir("t-json-index");
    let j = open_migrated(&dir, Config::default());
    // A handful of rows is enough for the planner to have a real table, but the
    // plan does not depend on cardinality: an index scan is chosen because the
    // predicate is an *exact* match on the indexed expression.
    let mut conn = Connection::open(j.path()).unwrap();
    let tx = conn.transaction().unwrap();
    for i in 0..200 {
        tx.execute(
            "INSERT INTO events
               (event_id, schema_version, source, session_id, sequence, ts_mono_ns,
                ts_wall, type, type_class, payload, evidence_status, ingested_mono_ns)
             VALUES (?1, 1, 'fs', NULL, 0, ?2, '2026-11-24T00:00:00.000Z',
                     'FileModified', 'fs.modify', ?3, 'observed', 0)",
            rusqlite::params![
                format!("e{i}"),
                i,
                format!(r#"{{"path":"/tmp/file{i}.rs"}}"#)
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();

    let plan = query_plan(
        &conn,
        "SELECT event_id FROM events WHERE json_extract(payload, '$.path') = ?1",
    );
    assert!(
        plan.contains("idx_events_payload_path"),
        "the expression index must be used, plan was:\n{plan}"
    );
    assert!(
        !plan.contains("SCAN events"),
        "a full table scan means the index is unusable, plan was:\n{plan}"
    );

    // And the same through the public API, so a future rewrite of the query in
    // read.rs cannot quietly lose the index.
    let reader = j.reader().unwrap();
    let hits = reader
        .events_touching_path("/tmp/file7.rs", 10)
        .expect("query");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, "FileModified");
}

fn query_plan(conn: &Connection, sql: &str) -> String {
    let mut st = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows: Vec<String> = st
        .query_map(["/tmp/file7.rs"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.join("\n")
}

/// The pragmas must be in effect on a real connection, not just named in a const.
#[test]
fn the_configured_pragmas_are_actually_in_effect() {
    let dir = fresh_dir("t-pragmas");
    let j = open_migrated(&dir, Config::default());
    let reader = j.reader().unwrap();
    let settings = li_journal::db::effective_settings(reader.connection()).expect("read pragmas");
    let get = |name: &str| {
        settings
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(get("journal_mode").to_lowercase(), "wal");
    assert_eq!(
        get("busy_timeout"),
        li_journal::BUSY_TIMEOUT_MS.to_string(),
        "busy_timeout must be the explicit value, not SQLite's default of 0"
    );
    assert_eq!(get("foreign_keys"), "1");
    // NORMAL is 1. 0-H §7.3 measured the kill -9 result under NORMAL, not FULL.
    assert_eq!(get("synchronous"), "1");
}

/// The default path is the one spec §2 names, and it is not guessed.
#[test]
fn the_default_path_matches_the_spec() {
    if let Some(p) = li_journal::db::default_path() {
        let s = p.to_string_lossy().to_string();
        assert!(
            s.ends_with(".local/state/linux-intelligence/journal.db"),
            "spec §2 names that path, got {s}"
        );
    }
}

/// The index the migration is built around must exist in **0001**, not in a later
/// migration: 0-H measured 160x for the query and ADR 0003 makes the late
/// addition expensive.
#[test]
fn the_json_index_is_created_by_migration_0001() {
    let dir = fresh_dir("t-json-index-v1");
    let mut j = li_journal::Journal::open(&dir.join("journal.db"), Config::default()).unwrap();
    // Stop at v1: if the index had arrived in v2, this would find nothing.
    j.migrate_to(1).unwrap();
    let conn = Connection::open(j.path()).unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_events_payload_path'",
            [],
            |r| r.get(0),
        )
        .expect("the index must exist after v1 alone");
    assert!(
        sql.contains("json_extract") && sql.contains("$.path"),
        "index body was: {sql}"
    );
}

/// P1-28: migration 0003 adds `idx_events_payload_new_path` and `query_events`
/// resolves `$.new_path` lookups via the expression index without full scans.
#[test]
fn the_new_path_json_index_is_used_for_rename_queries() {
    let dir = fresh_dir("t-new-path-index");
    let j = open_migrated(&dir, Config::default());
    let conn = Connection::open(j.path()).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    for i in 0..200 {
        tx.execute(
            "INSERT INTO events
               (event_id, schema_version, source, session_id, sequence, ts_mono_ns,
                ts_wall, type, type_class, payload, evidence_status, ingested_mono_ns)
             VALUES (?1, 1, 'fs', NULL, 0, ?2, '2026-11-24T00:00:00.000Z',
                     'FileRenamed', 'fs.rename', ?3, 'observed', 0)",
            rusqlite::params![
                format!("rn-{i}"),
                i,
                format!(r#"{{"path":"/tmp/old{i}.rs","new_path":"/tmp/new{i}.rs"}}"#)
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();

    let mut st = conn
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT event_id FROM events
             WHERE json_extract(payload, '$.path') = ?1
                OR json_extract(payload, '$.new_path') = ?1",
        )
        .unwrap();
    let plan: Vec<String> = st
        .query_map(["/tmp/new42.rs"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let joined = plan.join("\n");
    assert!(
        joined.contains("idx_events_payload_path")
            && joined.contains("idx_events_payload_new_path"),
        "expected both expression indexes in plan, got:\n{joined}"
    );
    assert!(
        !joined.contains("SCAN events"),
        "rename query must not fall back to SCAN events:\n{joined}"
    );

    let reader = j.reader().unwrap();
    let hits = reader
        .query_events(None, Some("/tmp/new42.rs"), None, 10)
        .expect("query_events by new_path");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].event_id, "rn-42");
    assert_eq!(hits[0].event_type, "FileRenamed");
}

/// 0-H §8.1.5: `caused_by` must be unrepresentable, and the DB is the last line.
#[test]
fn caused_by_cannot_be_stored_even_by_hand_written_sql() {
    let dir = fresh_dir("t-no-caused-by");
    let j = open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    for id in ["a", "b"] {
        j.submit(li_journal::write::Write::Event(
            li_journal::write::EventRow {
                event_id: id.into(),
                schema_version: 1,
                source: "fs".into(),
                session_id: None,
                process_row_id: None,
                sequence: 0,
                ts_mono_ns: 1,
                ts_wall: "2026-11-24T00:00:00.000Z".into(),
                type_: "FileModified".into(),
                type_class: "fs.modify".into(),
                payload: "{}".into(),
                evidence_status: "observed".into(),
                dedup_group: None,
                duplicate_of: None,
                redaction_rules: None,
                redaction_hits: None,
                redaction_digest: None,
                fp_type_class: None,
                fp_subject: None,
                fp_bucket_ns: None,
                ingested_mono_ns: 0,
            },
        ));
    }
    j.flush().unwrap();
    let conn = Connection::open(j.path()).unwrap();
    let err = conn
        .execute(
            "INSERT INTO event_relations
               (from_event, to_event, kind, level, rule, inferred_at_mono_ns)
             VALUES ('a', 'b', 'caused_by', 'confirmed', 'R3', 0)",
            [],
        )
        .expect_err("caused_by must be refused by CHECK");
    assert!(
        err.to_string().to_lowercase().contains("check"),
        "expected a CHECK failure, got: {err}"
    );
    // The three allowed kinds still work.
    conn.execute(
        "INSERT INTO event_relations
           (from_event, to_event, kind, level, rule, inferred_at_mono_ns)
         VALUES ('a', 'b', 'modified_during', 'weak', 'R6', 0)",
        [],
    )
    .unwrap();
}

/// 0-H §8.3.1: no `events.operation_id` column, ever.
#[test]
fn events_has_no_operation_id_column() {
    let dir = fresh_dir("t-no-opid");
    let j = open_migrated(&dir, Config::default());
    let conn = Connection::open(j.path()).unwrap();
    let mut st = conn.prepare("PRAGMA table_info(events)").unwrap();
    let cols: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        !cols.iter().any(|c| c == "operation_id"),
        "membership must be the join table; found {cols:?}"
    );
    assert!(cols.iter().any(|c| c == "ingest_seq"));
}

/// 0-H §8.3.2: `(pid, starttime)` must not be unique, and `ambiguous` must stick.
#[test]
fn two_incarnations_of_one_pid_and_starttime_can_coexist() {
    let dir = fresh_dir("t-pid-alias");
    let j = open_migrated(&dir, Config::default());
    for seq in 0..2 {
        j.submit(li_journal::write::Write::Process(
            li_journal::write::Process {
                pid: 4242,
                starttime: 99,
                incarnation_seq: seq,
                comm: Some("sh".into()),
                ppid: Some(1),
                session_id: None,
                first_seen_mono_ns: 1,
                ambiguous: seq == 1,
            },
        ));
    }
    j.flush().unwrap();
    let reader = j.reader().unwrap();
    assert_eq!(
        reader.count("processes").unwrap(),
        2,
        "a same-tick collision must be storable, not a constraint violation"
    );

    // Sticky: a later clean observation may not clear the flag.
    j.submit(li_journal::write::Write::Process(
        li_journal::write::Process {
            pid: 4242,
            starttime: 99,
            incarnation_seq: 1,
            comm: Some("sh".into()),
            ppid: Some(1),
            session_id: None,
            first_seen_mono_ns: 2,
            ambiguous: false,
        },
    ));
    j.flush().unwrap();
    let flagged: i64 = reader
        .connection()
        .query_row(
            "SELECT ambiguous FROM processes WHERE pid=4242 AND starttime=99 AND incarnation_seq=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(flagged, 1, "ambiguous must survive a clean re-observation");
}

/// An event must not be able to name a session that does not exist.
///
/// Two things are asserted, and the second is the one that matters at scale: the
/// row is refused, **and** the refusal does not take the rest of the batch with it.
#[test]
fn foreign_keys_reject_an_unknown_session() {
    let dir = fresh_dir("t-fk");
    let j = open_migrated(&dir, Config::default());
    j.submit(common::session("good"));
    // A bad row sandwiched between two good ones, in the same batch.
    j.event(&event(
        "e1",
        "good",
        0,
        "CommandStarted",
        serde_json::json!({}),
    ))
    .unwrap();
    j.event(&event(
        "e-bad",
        "no-such-session",
        0,
        "CommandStarted",
        serde_json::json!({}),
    ))
    .unwrap();
    j.event(&event(
        "e2",
        "good",
        1,
        "CommandFinished",
        serde_json::json!({}),
    ))
    .unwrap();
    let w = j.flush().unwrap();
    assert_eq!(w.failed, 1, "the writer must count the FK refusal");
    let reader = j.reader().unwrap();
    assert_eq!(
        reader.count("events").unwrap(),
        2,
        "the two valid events must survive their neighbour's refusal"
    );
    assert!(
        reader.payload_of("e-bad").unwrap().is_none(),
        "the refused event must not be stored"
    );
    let why = j.last_error().expect("the refusal must be reportable");
    assert!(why.contains("FOREIGN KEY"), "reason was: {why}");
}

/// `payload` must be valid JSON: a truncated payload is a corrupt fact.
#[test]
fn invalid_json_payload_is_refused() {
    let dir = fresh_dir("t-json-check");
    let j = open_migrated(&dir, Config::default());
    let conn = Connection::open(j.path()).unwrap();
    assert!(
        conn.execute(
            "INSERT INTO events
               (event_id, schema_version, source, sequence, ts_mono_ns, ts_wall,
                type, type_class, payload, evidence_status, ingested_mono_ns)
             VALUES ('x', 1, 'fs', 0, 0, '2026-11-24T00:00:00.000Z',
                     'FileModified', 'fs.modify', '{not json', 'observed', 0)",
            [],
        )
        .is_err()
    );
}

/// One event, two operations, two levels — the thing a single column cannot say.
#[test]
fn an_event_can_belong_to_two_operations_at_two_levels() {
    let dir = fresh_dir("t-membership");
    let j = open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    for op in ["op-edit", "op-recover"] {
        j.submit(li_journal::write::Write::Operation(
            li_journal::write::Operation {
                operation_id: op.to_string(),
                session_id: Some("s1".into()),
                label: op.to_string(),
                intent: None,
                state: "open".into(),
                started_mono_ns: 1,
            },
        ));
    }
    let ev = event(
        "e1",
        "s1",
        0,
        "FileModified",
        serde_json::json!({"path": "/tmp/x.rs"}),
    );
    j.event(&ev).unwrap();
    j.flush().unwrap();

    j.attach("e1", "op-edit", "R5", li_proto::CredibilityLevel::Probable);
    j.attach("e1", "op-recover", "R4", li_proto::CredibilityLevel::Strong);
    j.flush().unwrap();

    let reader = j.reader().unwrap();
    let edit = reader.operation_trail("op-edit").unwrap();
    let recover = reader.operation_trail("op-recover").unwrap();
    assert_eq!(
        edit,
        vec![(
            "e1".to_string(),
            "FileModified".to_string(),
            "probable".to_string()
        )]
    );
    assert_eq!(recover[0].2, "strong");
    assert_eq!(reader.count("event_operations").unwrap(), 2);
}
