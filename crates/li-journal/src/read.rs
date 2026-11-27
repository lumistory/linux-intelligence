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

use serde::Serialize;

/// Read-only projection of a stored event, always carrying `evidence_status`.
#[derive(Debug, Clone, Serialize)]
pub struct EventRecord {
    pub ingest_seq: i64,
    pub event_id: String,
    pub source: String,
    pub session_id: Option<String>,
    pub sequence: i64,
    pub ts_mono_ns: i64,
    pub ts_wall: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub type_class: String,
    pub evidence_status: String,
    pub payload: serde_json::Value,
    pub redaction_rules: Option<String>,
}

/// Read-only projection of a stored relation, always carrying its credibility level.
#[derive(Debug, Clone, Serialize)]
pub struct RelationRecord {
    pub relation_id: i64,
    pub from_event: String,
    pub to_event: String,
    pub kind: String,
    pub credibility_level: String,
    pub rule: String,
    pub inferred_at_mono_ns: i64,
}

/// Read-only projection of a stored session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub parent_session: Option<String>,
    pub adapter: String,
    pub shell_pid: Option<i64>,
    pub started_wall: String,
    pub ended_wall: Option<String>,
    pub boot_id: Option<String>,
}

/// Read-only projection of a stored checkpoint.
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointRecord {
    pub checkpoint_id: String,
    pub session_id: Option<String>,
    pub operation_id: Option<String>,
    pub created_wall: String,
    pub kind: String,
    pub reference: Option<String>,
    pub file_count: Option<i64>,
    pub note: Option<String>,
}

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

    /// Queries events ordered by `ingest_seq` descending, optionally filtered by
    /// session, `payload->>'$.path'`, or `type`. Every row carries `evidence_status`.
    pub fn query_events(
        &self,
        session_id: Option<&str>,
        path: Option<&str>,
        event_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<EventRecord>> {
        let mut sql = String::from(
            "SELECT ingest_seq, event_id, source, session_id, sequence, ts_mono_ns,
                    ts_wall, type, type_class, evidence_status, payload, redaction_rules
             FROM events WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(s) = session_id {
            sql.push_str(" AND session_id = ?");
            params.push(Box::new(s.to_string()));
        }
        if let Some(p) = path {
            // Exact expression matching `idx_events_payload_path` (0-H §7.4).
            sql.push_str(" AND json_extract(payload, '$.path') = ?");
            params.push(Box::new(p.to_string()));
        }
        if let Some(t) = event_type {
            sql.push_str(" AND type = ?");
            params.push(Box::new(t.to_string()));
        }
        sql.push_str(" ORDER BY ingest_seq DESC LIMIT ?");
        params.push(Box::new(limit.max(1)));

        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let mut st = self.conn.prepare(&sql)?;
        let rows = st
            .query_map(refs.as_slice(), |r| {
                let payload_raw: String = r.get(10)?;
                let payload: serde_json::Value =
                    serde_json::from_str(&payload_raw).unwrap_or(serde_json::Value::Null);
                Ok(EventRecord {
                    ingest_seq: r.get(0)?,
                    event_id: r.get(1)?,
                    source: r.get(2)?,
                    session_id: r.get(3)?,
                    sequence: r.get(4)?,
                    ts_mono_ns: r.get(5)?,
                    ts_wall: r.get(6)?,
                    event_type: r.get(7)?,
                    type_class: r.get(8)?,
                    evidence_status: r.get(9)?,
                    payload,
                    redaction_rules: r.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Queries stored relations, each carrying its credibility level.
    pub fn query_relations(
        &self,
        event_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<RelationRecord>> {
        let (sql, param): (&str, Option<String>) = match event_id {
            Some(eid) => (
                "SELECT relation_id, from_event, to_event, kind, level, rule, inferred_at_mono_ns
                 FROM event_relations
                 WHERE from_event = ?1 OR to_event = ?1
                 ORDER BY relation_id DESC LIMIT ?2",
                Some(eid.to_string()),
            ),
            None => (
                "SELECT relation_id, from_event, to_event, kind, level, rule, inferred_at_mono_ns
                 FROM event_relations
                 ORDER BY relation_id DESC LIMIT ?1",
                None,
            ),
        };
        let mut st = self.conn.prepare(sql)?;
        let map_row = |r: &rusqlite::Row<'_>| {
            Ok(RelationRecord {
                relation_id: r.get(0)?,
                from_event: r.get(1)?,
                to_event: r.get(2)?,
                kind: r.get(3)?,
                credibility_level: r.get(4)?,
                rule: r.get(5)?,
                inferred_at_mono_ns: r.get(6)?,
            })
        };
        let rows = match param {
            Some(eid) => st
                .query_map(rusqlite::params![eid, limit.max(1)], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            None => st
                .query_map(rusqlite::params![limit.max(1)], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    pub fn list_sessions(&self, limit: i64) -> Result<Vec<SessionRecord>> {
        let mut st = self.conn.prepare_cached(
            "SELECT session_id, parent_session, adapter, shell_pid, started_wall, ended_wall, boot_id
             FROM sessions
             ORDER BY started_mono_ns DESC LIMIT ?1",
        )?;
        let rows = st
            .query_map([limit.max(1)], |r| {
                Ok(SessionRecord {
                    session_id: r.get(0)?,
                    parent_session: r.get(1)?,
                    adapter: r.get(2)?,
                    shell_pid: r.get(3)?,
                    started_wall: r.get(4)?,
                    ended_wall: r.get(5)?,
                    boot_id: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_checkpoints(&self, limit: i64) -> Result<Vec<CheckpointRecord>> {
        let mut st = self.conn.prepare_cached(
            "SELECT checkpoint_id, session_id, operation_id, created_wall, kind, reference, file_count, note
             FROM checkpoints
             ORDER BY created_wall DESC LIMIT ?1",
        )?;
        let rows = st
            .query_map([limit.max(1)], |r| {
                Ok(CheckpointRecord {
                    checkpoint_id: r.get(0)?,
                    session_id: r.get(1)?,
                    operation_id: r.get(2)?,
                    created_wall: r.get(3)?,
                    kind: r.get(4)?,
                    reference: r.get(5)?,
                    file_count: r.get(6)?,
                    note: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
