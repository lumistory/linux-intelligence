//! Read side. A separate, **read-only** connection per `Reader`, because WAL lets
//! readers run concurrently with the single writer (0-H §7.2: 0 busy retries for
//! four readers against a hot writer).
//!
//! Read-only is enforced by the open flags, not by discipline: a query cannot
//! accidentally become the second writer.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::db::Config;
use crate::error::{JournalError, Result};

pub struct Reader {
    conn: Connection,
    path: PathBuf,
}

impl Reader {
    pub fn open(path: &Path, config: &Config) -> Result<Reader> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        // The per-connection pragmas are applied here too. They are not inherited
        // from the writer and one of them (`foreign_keys`) is needed by
        // `foreign_key_check`, while another (`synchronous`) would otherwise
        // default to FULL and make this reader a different configuration than the
        // writer it is reading.
        crate::db::apply_per_connection_pragmas(&conn, config)?;
        Ok(Reader {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// `ok` or a list of problems. This is spec §6 criterion 4.
    pub fn integrity_check(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?)
    }

    /// Rows that violate a foreign key. Any value but 0 is a bug.
    pub fn foreign_key_violations(&self) -> Result<usize> {
        let mut st = self.conn.prepare("PRAGMA foreign_key_check")?;
        Ok(st.query_map([], |_| Ok(()))?.count())
    }

    pub fn count(&self, table: &str) -> Result<i64> {
        // Table names are not bindable parameters; the allow-list keeps this from
        // being an injection point if a caller ever passes user text.
        const ALLOWED: &[&str] = &[
            "events",
            "sessions",
            "processes",
            "operations",
            "event_operations",
            "event_relations",
            "evidence",
            "checkpoints",
            "recovery_plans",
            "approvals",
            "adapter_registry",
            "schema_version",
        ];
        if !ALLOWED.contains(&table) {
            return Err(JournalError::Integrity(format!(
                "`{table}` is not a journal table"
            )));
        }
        Ok(self
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }

    /// Events of one session in producer order.
    pub fn events_of_session(&self, session_id: &str, limit: i64) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT event_id, sequence FROM events
             WHERE session_id = ?1 ORDER BY sequence LIMIT ?2",
        )?;
        let rows = st
            .query_map(rusqlite::params![session_id, limit], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// **The shape the JSON expression index exists for** (0-H §7.4): the events
    /// that name this path.
    ///
    /// The predicate must match `idx_events_payload_path`'s expression
    /// *syntactically* — `json_extract(payload, '$.path')` — or SQLite will not
    /// use the index. `tests/schema.rs` asserts the plan, so a rewrite of this
    /// query that quietly loses the index fails the suite.
    pub fn events_touching_path(&self, path: &str, limit: i64) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT e.event_id, e.type FROM events e
             WHERE json_extract(e.payload, '$.path') = ?1
             ORDER BY e.ingest_seq DESC LIMIT ?2",
        )?;
        let rows = st
            .query_map(rusqlite::params![path, limit], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// "What did this operation touch, and how sure are we?" — one join.
    pub fn operation_trail(&self, operation_id: &str) -> Result<Vec<(String, String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT e.event_id, e.type, eo.level
             FROM event_operations eo
             JOIN events e ON e.event_id = eo.event_id
             WHERE eo.operation_id = ?1 AND eo.detached_mono_ns IS NULL
             ORDER BY e.ts_mono_ns",
        )?;
        let rows = st
            .query_map([operation_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Stored payload of one event, for tests and `li query`.
    pub fn payload_of(&self, event_id: &str) -> Result<Option<String>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT payload FROM events WHERE event_id = ?1")?;
        let mut rows = st.query([event_id])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Per-session gap report: producer `sequence` values present vs the span.
    ///
    /// A gap is **data, not an error** (C1/C2): it is the trace of a drop that was
    /// counted somewhere. Reporting it is what makes loss auditable instead of
    /// invisible. The returned pairs are inclusive `(first_missing, last_missing)`.
    pub fn sequence_gaps(&self, session_id: &str) -> Result<Vec<(i64, i64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT sequence FROM events WHERE session_id = ?1 ORDER BY sequence",
        )?;
        let seqs = st
            .query_map([session_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut gaps = Vec::new();
        for pair in seqs.windows(2) {
            if pair[1] > pair[0] + 1 {
                gaps.push((pair[0] + 1, pair[1] - 1));
            }
        }
        Ok(gaps)
    }
}
