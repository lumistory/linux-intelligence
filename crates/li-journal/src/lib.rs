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
use crate::write::{EventRow, OperationEdge, Relation, Watermark, Write};
use crate::writer::Writer;

pub use crate::db::{BUSY_TIMEOUT_MS, Config};
pub use crate::migrate::{MIGRATIONS, Migration, current_version, known_version};
pub use crate::read::Reader;
pub use crate::writer::Accepted;

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
}
