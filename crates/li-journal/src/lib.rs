//! SQLite journal: schema, forward-only migrations, append-only writer.
//!
//! # What this crate guarantees, and where each guarantee is enforced
//!
//! | guarantee | enforced by |
//! |---|---|
//! | `events` cannot be rewritten | `BEFORE UPDATE`/`BEFORE DELETE` triggers that `RAISE(ABORT)` — a database object, not a code path |
//! | one writer | the only read-write `Connection` is owned by the writer thread; [`Journal::submit`] is a channel send |
//! | secrets never reach the file | [`redact`] runs before a [`write::EventRow`] exists, so an unredacted payload has no route to `INSERT` |
//! | forward-only schema | [`migrate`] applies upward only and verifies checksums instead of re-applying |
//! | a crash leaves a usable file | WAL + `synchronous=NORMAL`, verified by a real `SIGKILL` in `tests/crash.rs` |
//!
//! # Shaped for `li-daemon`
//!
//! [`Journal`] is not `Clone`: the writer thread is a resource, and the daemon
//! should hold one and call [`submit`](Journal::submit) from any task. Reading
//! gets its own [`Reader`], which cannot write.

pub mod clock;
pub mod db;
pub mod error;
pub mod migrate;
pub mod read;
pub mod redact;
pub mod write;
mod writer;

use std::path::{Path, PathBuf};

use crate::error::{JournalError, Result};
use crate::redact::MASK;
use crate::write::{
    AdapterRecord, Checkpoint, EventRow, OperationEdge, Relation, Session, SessionEnd, Watermark,
    Write,
};
use crate::writer::Writer;

pub use crate::db::{BUSY_TIMEOUT_MS, Config};
pub use crate::migrate::{MIGRATIONS, Migration, current_version, known_version};
pub use crate::read::{CheckpointRecord, EventRecord, Reader, RelationRecord, SessionRecord};
pub use crate::writer::Accepted;

/// Result of an audited retention purge (`ADR 0003` / P1-26).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeStats {
    pub deleted_events: u64,
    pub audit_event_id: String,
}

/// The journal: one database file, one writer thread.
pub struct Journal {
    path: PathBuf,
    config: Config,
    writer: Writer,
}

impl Journal {
    /// Opens (creating if needed) and configures the database, then starts the
    /// writer thread.
    ///
    /// Migration is **not** run here on purpose: a caller must be able to open a
    /// database and read its version before deciding to change it, which is what
    /// makes the "vN+1 over a populated vN" test possible at all.
    pub fn open(path: &Path, config: Config) -> Result<Journal> {
        let conn = db::open(path, &config)?;
        Ok(Journal {
            path: path.to_path_buf(),
            writer: Writer::spawn(conn, config.clone()),
            config,
        })
    }

    pub fn open_default() -> Result<Journal> {
        let path = db::default_path()
            .ok_or_else(|| JournalError::Integrity("HOME is unset or not absolute".to_string()))?;
        Journal::open(&path, Config::default())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Applies every pending migration on a **separate** connection.
    ///
    /// Separate on purpose: the writer thread already owns a connection to this
    /// file, and running DDL through it would contend with ingest. Putting
    /// migration on its own connection makes "migrate before ingest" structural
    /// instead of remembered.
    pub fn migrate(&mut self) -> Result<Vec<i64>> {
        self.migrate_to(known_version())
    }

    pub fn migrate_to(&mut self, target: i64) -> Result<Vec<i64>> {
        // Drain the writer first, so a migration never lands mid-batch.
        let _ = self.writer.flush();
        let mut conn = db::open(&self.path, &self.config)?;
        migrate::migrate(&mut conn, Some(target))
    }

    /// Hands a row to the writer. Never blocks on the disk.
    ///
    /// Returns [`Accepted::Dropped`] when the bounded queue is full (C1) and the
    /// drop is counted, so loss is visible in the next [`flush`](Journal::flush)
    /// rather than becoming memory growth or a stalled caller.
    pub fn submit(&self, row: Write) -> Accepted {
        self.writer.submit(row)
    }

    /// Commits everything queued before this call and returns the watermark.
    ///
    /// This is delivery proof (C2): a per-event ack is not.
    pub fn flush(&self) -> Result<Watermark> {
        self.writer.flush()
    }

    /// The last row-level refusal, if any. This is what turns `failed > 0` in a
    /// watermark into something an operator can act on.
    pub fn last_error(&self) -> Option<String> {
        self.writer.last_error()
    }

    /// Redacts, marshals and submits one protocol event.
    ///
    /// Redaction happens here — before the row exists — so the unredacted form is
    /// never built into anything that could be written. Returns the redaction
    /// outcome so a caller can log what fired without logging the secret.
    pub fn event(&self, event: &li_proto::Event) -> Result<(Accepted, Option<redact::Redaction>)> {
        let (row, redaction) = self.event_row(event)?;
        Ok((self.submit(Write::Event(row)), redaction))
    }

    fn event_row(&self, event: &li_proto::Event) -> Result<(EventRow, Option<redact::Redaction>)> {
        let mut payload = event.payload.clone();
        let redaction = redact::redact_payload(&mut payload);
        let payload = serde_json::to_string(&payload)
            .map_err(|e| JournalError::Integrity(format!("payload re-encode: {e}")))?;
        let fp = event.semantic_fingerprint.as_ref();
        let mut row = EventRow {
            event_id: event.event_id.as_str().to_string(),
            schema_version: i64::try_from(event.schema_version.get()).map_err(|_| {
                JournalError::NotRepresentable {
                    field: "schema_version",
                }
            })?,
            source: event.source.as_str().to_string(),
            session_id: event.session_id.as_ref().map(|s| s.as_str().to_string()),
            process_row_id: None,
            sequence: i64::try_from(event.sequence)
                .map_err(|_| JournalError::NotRepresentable { field: "sequence" })?,
            ts_mono_ns: i64::try_from(event.ts_mono_ns).map_err(|_| {
                JournalError::NotRepresentable {
                    field: "ts_mono_ns",
                }
            })?,
            ts_wall: event.ts_wall.clone(),
            type_: event.event_type.as_str().to_string(),
            type_class: type_class(event.event_type.as_str()).to_string(),
            payload,
            evidence_status: event.evidence_status.as_str().to_string(),
            dedup_group: None,
            duplicate_of: None,
            redaction_rules: None,
            redaction_hits: None,
            redaction_digest: None,
            fp_type_class: fp.map(|f| f.type_class.clone()),
            fp_subject: fp.map(|f| f.subject.clone()),
            fp_bucket_ns: fp.map(|f| f.bucket_ns),
            ingested_mono_ns: clock::mono_ns().unwrap_or(0) as i64,
        };
        if let Some(r) = &redaction {
            row.apply_redaction(r);
        }
        Ok((row, redaction))
    }

    /// Adds an event to an operation at a given credibility level (0-H §8.3.1).
    pub fn attach(
        &self,
        event_id: &str,
        operation_id: &str,
        attached_by: &str,
        level: li_proto::CredibilityLevel,
    ) -> Accepted {
        self.submit(Write::OperationEdge(OperationEdge {
            event_id: event_id.to_string(),
            operation_id: operation_id.to_string(),
            attached_by: attached_by.to_string(),
            level: level.as_str().to_string(),
            attached_mono_ns: clock::mono_ns().unwrap_or(0) as i64,
        }))
    }

    /// Records a relation. `kind` is checked by the schema, so `caused_by` is
    /// refused by SQLite even if a caller asks for it.
    pub fn relate(
        &self,
        from_event: &str,
        to_event: &str,
        kind: &str,
        level: li_proto::CredibilityLevel,
        rule: &str,
    ) -> Accepted {
        self.submit(Write::Relation(Relation {
            from_event: from_event.to_string(),
            to_event: to_event.to_string(),
            kind: kind.to_string(),
            level: level.as_str().to_string(),
            rule: rule.to_string(),
            inferred_at_mono_ns: clock::mono_ns().unwrap_or(0) as i64,
        }))
    }

    /// Records a checkpoint row through the single writer.
    pub fn checkpoint(&self, cp: Checkpoint) -> Accepted {
        self.submit(Write::Checkpoint(cp))
    }

    /// Records or updates an adapter's declared capability record.
    pub fn adapter(&self, rec: AdapterRecord) -> Accepted {
        self.submit(Write::Adapter(rec))
    }

    /// Registers a session through the single writer.
    pub fn session(&self, session: Session) -> Accepted {
        self.submit(Write::Session(session))
    }

    /// Marks the end time of an existing session.
    pub fn session_end(&self, end: SessionEnd) -> Accepted {
        self.submit(Write::SessionEnd(end))
    }

    /// Audited retention purge (ADR 0003, P1-26).
    ///
    /// Drains the writer, opens a dedicated `BEGIN IMMEDIATE` transaction, drops
    /// the append-only triggers for the scope of the transaction only, deletes
    /// `events` older than `cutoff_wall` (with dangling `duplicate_of` refs cleared
    /// first), restores both triggers before commit, and inserts an `EventsPurged`
    /// audit event in the same atomic transaction.
    pub fn purge_before(&self, cutoff_wall: &str, reason: &str) -> Result<PurgeStats> {
        if cutoff_wall.len() != 24 || !cutoff_wall.ends_with('Z') {
            return Err(JournalError::Integrity(
                "cutoff_wall must be 24-byte RFC 3339 UTC (`YYYY-MM-DDTHH:MM:SS.mmmZ`)".into(),
            ));
        }
        self.writer.flush()?;
        let conn = db::open(&self.path, &self.config)?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let res = (|| -> Result<PurgeStats> {
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS events_append_only_update;
                 DROP TRIGGER IF EXISTS events_append_only_delete;",
            )?;
            // Clear self-referential `duplicate_of` links pointing into the purged slice
            // so `PRAGMA foreign_keys=ON` does not reject the DELETE.
            conn.execute(
                "UPDATE events SET duplicate_of = NULL
                 WHERE duplicate_of IN (SELECT event_id FROM events WHERE ts_wall < ?1)",
                [cutoff_wall],
            )?;
            let deleted = conn.execute("DELETE FROM events WHERE ts_wall < ?1", [cutoff_wall])?;
            conn.execute_batch(
                "CREATE TRIGGER events_append_only_update BEFORE UPDATE ON events
                 BEGIN
                     SELECT RAISE(ABORT, 'append_only: events.UPDATE is rejected');
                 END;
                 CREATE TRIGGER events_append_only_delete BEFORE DELETE ON events
                 BEGIN
                     SELECT RAISE(ABORT, 'append_only: events.DELETE is rejected');
                 END;",
            )?;
            let now_mono = clock::mono_ns().unwrap_or(0) as i64;
            let now_wall = clock::wall_now_rfc3339_ms();
            let audit_id = format!("events-purged-{}-{now_mono}", std::process::id());
            let payload = serde_json::json!({
                "cutoff_wall": cutoff_wall,
                "deleted_events": deleted,
                "reason": reason,
            })
            .to_string();
            conn.execute(
                "INSERT INTO events
                   (event_id, schema_version, source, session_id, process_row_id,
                    sequence, ts_mono_ns, ts_wall, type, type_class, payload,
                    evidence_status, ingested_mono_ns)
                 VALUES (?1, 1, 'policy', NULL, NULL, 0, ?2, ?3, 'EventsPurged',
                         'other', ?4, 'observed', ?2)",
                rusqlite::params![audit_id, now_mono, now_wall, payload],
            )?;
            Ok(PurgeStats {
                deleted_events: deleted as u64,
                audit_event_id: audit_id,
            })
        })();
        match res {
            Ok(stats) => {
                conn.execute_batch("COMMIT")?;
                Ok(stats)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Opens a read-only view. Independent of the writer, so reads never queue
    /// behind ingest.
    pub fn reader(&self) -> Result<Reader> {
        Reader::open(&self.path, &self.config)
    }
}

/// Coarse class the dedup layer keys on.
///
/// Kept here rather than in `li-proto` because it is a *storage* concern: the
/// wire carries the precise `type`, and the class only exists to let two
/// different types describe one fact (`CommandFinished` vs `ProcessExited`). The
/// mapping matches 0-H's `type_class`, so rows from both stay comparable.
pub fn type_class(event_type: &str) -> &'static str {
    match event_type {
        // `ProcessExec` is deliberately absent: exec is not creation, and merging
        // them would assert a fork edge nobody observed. It falls to `other`.
        "CommandStarted" | "ProcessCreated" | "ProcessStart" => "proc.start",
        "CommandFinished" | "ProcessExited" | "ProcessExit" => "proc.end",
        "FileModified" => "fs.modify",
        "FileCreated" => "fs.create",
        "FileDeleted" => "fs.delete",
        "FileRenamed" => "fs.rename",
        "SessionStarted" | "SessionStart" => "session.start",
        "SessionEnded" | "SessionEnd" => "session.end",
        _ => "other",
    }
}

/// The mask token, re-exported so a test can assert on it without duplicating a
/// literal that could drift.
pub const REDACTION_MASK: &str = MASK;

/// Convenience for a temporary database path. Test and tooling only; the daemon
/// uses [`db::default_path`].
pub fn temp_path(unique: &str) -> PathBuf {
    std::env::temp_dir()
        .join("li-journal-tests")
        .join(format!("{unique}.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_classes_match_the_0_h_prototype() {
        // The point of the mapping: a hook's CommandFinished and a poller's
        // ProcessExited must land in the same class, or the dedup layer cannot
        // see them as one fact.
        assert_eq!(type_class("CommandFinished"), type_class("ProcessExited"));
        assert_eq!(type_class("CommandFinished"), "proc.end");
        assert_eq!(type_class("FileModified"), "fs.modify");
        // An unknown type is stored and classified `other`, never refused.
        assert_eq!(type_class("SomethingFromTheFuture"), "other");
        // ProcessExec is not creation: merging them asserts a fork edge nobody
        // observed (0-H §4.4).
        assert_ne!(type_class("ProcessExec"), type_class("ProcessCreated"));
    }

    #[test]
    fn checkpoints_queries_and_audited_purge_work_while_preserving_triggers() {
        let path = temp_path("purge-and-query");
        let _ = std::fs::remove_file(&path);
        let mut j = Journal::open(&path, Config::default()).unwrap();
        j.migrate().unwrap();

        j.session(Session {
            session_id: "s-purge".into(),
            parent_session: None,
            adapter: "bash".into(),
            adapter_version: None,
            shell_pid: Some(123),
            started_wall: "2026-11-24T00:00:00.000Z".into(),
            started_mono_ns: 1,
            cwd: None,
            host: None,
            boot_id: None,
        });
        for (idx, wall) in [
            (0u64, "2026-11-24T00:00:01.000Z"),
            (1u64, "2026-11-24T12:00:00.000Z"),
        ] {
            let mut payload = serde_json::Map::new();
            payload.insert("path".into(), serde_json::json!("/tmp/demo"));
            let ev = li_proto::Event::new(
                li_proto::EventId::new(format!("ev-purge-{idx}")).unwrap(),
                li_proto::Source::ShellBash,
                Some(li_proto::SessionId::new("s-purge").unwrap()),
                idx,
                idx + 10,
                wall,
                li_proto::EventType::new("CommandFinished").unwrap(),
                payload,
                li_proto::EvidenceStatus::Declared,
            )
            .unwrap();
            j.event(&ev).unwrap();
        }
        j.checkpoint(Checkpoint {
            checkpoint_id: "cp-1".into(),
            session_id: Some("s-purge".into()),
            operation_id: None,
            created_wall: "2026-11-24T00:00:02.000Z".into(),
            kind: "manual".into(),
            reference: Some("ref-1".into()),
            file_count: Some(1),
            note: Some("note".into()),
        });
        j.flush().unwrap();

        let r = j.reader().unwrap();
        assert_eq!(
            r.query_events(Some("s-purge"), Some("/tmp/demo"), None, 10)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(r.list_checkpoints(10).unwrap().len(), 1);
        drop(r);

        let stats = j
            .purge_before("2026-11-24T06:00:00.000Z", "retention_test")
            .unwrap();
        assert_eq!(stats.deleted_events, 1);

        let r = j.reader().unwrap();
        let remaining = r.query_events(None, None, None, 10).unwrap();
        assert_eq!(remaining.len(), 2); // ev-purge-1 + EventsPurged audit row
        assert!(remaining.iter().any(|e| e.event_type == "EventsPurged"));
        assert_eq!(r.integrity_check().unwrap(), "ok");
        drop(r);

        // Append-only trigger must be back in place after purge.
        let conn = db::open(&path, &Config::default()).unwrap();
        assert!(conn.execute("DELETE FROM events", []).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
