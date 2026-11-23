//! SQLite store: schema migration, ingest, the three query shapes, retention.
//!
//! Deliberate choices, each defended by a measurement in `results/`:
//!   * WAL, `synchronous=NORMAL`: readers never block the writer (test WAL-1).
//!   * `busy_timeout` is set explicitly, and its value is reported, because the
//!     default (0) turns ordinary contention into `SQLITE_BUSY` errors.
//!   * `foreign_keys=ON` per connection: SQLite defaults it OFF, so a prototype
//!     that forgets it "passes" while orphaning rows.
//!   * migrations are files with a checksum, applied in version order, and the
//!     applied set is recorded in `schema_version`.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::util::fnv_hex;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("migration {file} is already applied with a different checksum")]
    MigrationDrift { file: String },
    #[error("migration {file} has no schema_version statement")]
    MigrationUnversioned { file: String },
    #[error("database is at v{found}, cannot migrate down to v{target}")]
    Downgrade { found: i64, target: i64 },
    #[error("integrity check failed: {0}")]
    Integrity(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone, Copy)]
pub struct DbConfig {
    pub busy_timeout_ms: u64,
    pub synchronous_normal: bool,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            busy_timeout_ms: 5_000,
            synchronous_normal: true,
        }
    }
}

pub struct Store {
    pub conn: Connection,
    pub path: PathBuf,
    pub config: DbConfig,
}

impl Store {
    pub fn open(path: impl AsRef<Path>, config: DbConfig) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        Self::configure(&conn, config)?;
        Ok(Self { conn, path, config })
    }

    /// Read-only connection for the concurrent-reader test.
    pub fn open_reader(path: impl AsRef<Path>, busy_timeout_ms: u64) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(conn)
    }

    fn configure(conn: &Connection, config: DbConfig) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_millis(config.busy_timeout_ms))?;
        // WAL is persistent in the file, but setting it is idempotent and re-
        // asserting it catches a database created by another tool.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(DbError::Integrity(format!(
                "journal_mode is {mode}, not wal"
            )));
        }
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        if config.synchronous_normal {
            conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        }
        // 64 MiB page cache; measured effect is in the perf table.
        conn.execute_batch("PRAGMA cache_size=-65536;")?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        if !self.has_migration_table()? {
            return Ok(0);
        }
        let n: Option<i64> = self
            .conn
            .query_row("SELECT max(version) FROM schema_version", [], |r| r.get(0))
            .optional_first()?;
        Ok(n.unwrap_or(0))
    }

    /// A brand-new database has no `schema_version` table yet: migration 0001
    /// creates it, so the bookkeeping table cannot be assumed before v1.
    fn has_migration_table(&self) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn applied_migrations(&self) -> Result<Vec<(i64, String, String)>> {
        if !self.has_migration_table()? {
            return Ok(Vec::new());
        }
        let mut st = self.conn.prepare(
            "SELECT version, description, checksum FROM schema_version ORDER BY version",
        )?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Apply every migration with version <= `target`, in order.
    pub fn migrate(&mut self, dir: impl AsRef<Path>, target: Option<i64>) -> Result<Vec<i64>> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "sql"))
            .collect();
        files.sort();

        let mut applied_versions: Vec<i64> = self
            .applied_migrations()?
            .into_iter()
            .map(|(v, _, _)| v)
            .collect();
        let mut applied_now = Vec::new();

        for path in files {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let Some(version) = parse_version(&name) else {
                return Err(DbError::MigrationUnversioned { file: name });
            };
            if let Some(t) = target {
                if version > t {
                    continue;
                }
            }
            let body = std::fs::read_to_string(&path)?;
            let checksum = fnv_hex(&body);

            if applied_versions.contains(&version) {
                // Verify, do not re-apply: a changed file with the same version
                // is a bug in the migration series, not something to "fix".
                let existing: String = self.conn.query_row(
                    "SELECT checksum FROM schema_version WHERE version=?1",
                    [version],
                    |r| r.get(0),
                )?;
                if existing != checksum {
                    return Err(DbError::MigrationDrift { file: name });
                }
                continue;
            }
            if let Some(current) = applied_versions.iter().max().copied() {
                if version < current {
                    return Err(DbError::Downgrade {
                        found: current,
                        target: version,
                    });
                }
            }

            // DDL plus the bookkeeping row in one transaction: a migration is
            // either fully applied and recorded, or not at all.
            let tx = self.conn.transaction()?;
            tx.execute_batch(&body)?;
            tx.execute(
                "INSERT INTO schema_version (version, applied_at, description, checksum)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    version,
                    crate::util::now_wall(),
                    name.trim_end_matches(".sql"),
                    checksum
                ],
            )?;
            tx.commit()?;
            applied_versions.push(version);
            applied_now.push(version);
        }
        Ok(applied_now)
    }

    pub fn integrity_check(&self) -> Result<String> {
        let r: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        Ok(r)
    }

    pub fn foreign_key_violations(&self) -> Result<usize> {
        let mut st = self.conn.prepare("PRAGMA foreign_key_check")?;
        let n = st.query_map([], |_| Ok(()))?.count();
        Ok(n)
    }

    pub fn wal_pages(&self) -> Result<(i64, i64)> {
        let wal: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(0))
            .or_else(|_| Ok::<i64, rusqlite::Error>(0))?;
        let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        Ok((wal, pages))
    }

    // ---------------------------------------------------------------- ingest

    pub fn upsert_session(
        &self,
        session_id: &str,
        adapter: &str,
        adapter_version: Option<&str>,
        started_wall: &str,
        started_mono_ns: i64,
        cwd: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions
               (session_id, adapter, adapter_version, started_wall, started_mono_ns, cwd, host)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id) DO NOTHING",
            rusqlite::params![
                session_id,
                adapter,
                adapter_version,
                started_wall,
                started_mono_ns,
                cwd,
                std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into())
            ],
        )?;
        Ok(())
    }

    pub fn insert_process(
        &self,
        pid: u32,
        starttime: u64,
        incarnation_seq: u32,
        comm: &str,
        ppid: u32,
        session_id: Option<&str>,
        first_seen_mono_ns: i64,
        ambiguous: bool,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO processes
               (pid, starttime, incarnation_seq, comm, ppid, session_id,
                first_seen_mono_ns, ambiguous)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(pid, starttime, incarnation_seq) DO UPDATE SET
                last_seen_mono_ns = excluded.first_seen_mono_ns,
                ambiguous = max(processes.ambiguous, excluded.ambiguous)",
            rusqlite::params![
                pid,
                starttime,
                incarnation_seq,
                comm,
                ppid,
                session_id,
                first_seen_mono_ns,
                i64::from(ambiguous)
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn process_row_id(&self, pid: u32, starttime: u64, seq: u32) -> Result<Option<i64>> {
        let mut st = self.conn.prepare_cached(
            "SELECT process_row_id FROM processes
             WHERE pid=?1 AND starttime=?2 AND incarnation_seq=?3",
        )?;
        let mut rows = st.query(rusqlite::params![pid, starttime, seq])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_event(&self, e: &EventRow<'_>) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO events
               (event_id, schema_version, source, session_id, process_row_id, sequence,
                ts_mono_ns, ts_wall, type, type_class, payload, evidence_status,
                dedup_group, duplicate_of, redaction_digest, ingested_mono_ns)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(event_id) DO NOTHING",
            rusqlite::params![
                e.event_id,
                e.schema_version,
                e.source,
                e.session_id,
                e.process_row_id,
                e.sequence,
                e.ts_mono_ns,
                e.ts_wall,
                e.type_,
                e.type_class,
                e.payload,
                e.evidence_status,
                e.dedup_group,
                e.duplicate_of,
                e.redaction_digest,
                e.ingested_mono_ns,
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn insert_relation(
        &self,
        from: &str,
        to: &str,
        kind: &str,
        level: &str,
        rule: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO event_relations
               (from_event, to_event, kind, level, rule, inferred_at_mono_ns)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(from_event, to_event, kind) DO UPDATE SET
                level=excluded.level, rule=excluded.rule",
            rusqlite::params![
                from,
                to,
                kind,
                level,
                rule,
                crate::util::now_mono_ns() as i64
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_evidence(
        &self,
        relation_id: i64,
        source: &str,
        source_class: &str,
        status: &str,
        independent: bool,
        detail: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO evidence
               (relation_id, source, source_class, evidence_status, independent, confidence, detail)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                relation_id,
                source,
                source_class,
                status,
                i64::from(independent),
                1.0,
                detail
            ],
        )?;
        Ok(())
    }

    // --------------------------------------------------------------- queries

    pub fn events_of_session(
        &self,
        session_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT event_id, sequence, type FROM events
             WHERE session_id=?1 ORDER BY sequence LIMIT ?2",
        )?;
        let rows = st
            .query_map(rusqlite::params![session_id, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn events_in_range(&self, lo: i64, hi: i64) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT event_id, ts_mono_ns FROM events
             WHERE ts_mono_ns >= ?1 AND ts_mono_ns < ?2 ORDER BY ts_mono_ns",
        )?;
        let rows = st
            .query_map(rusqlite::params![lo, hi], |r| Ok((r.get(0)?, r.get(1)?)))?
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

    /// Correlated facts about a file: relations whose endpoints name a path.
    pub fn file_trail(&self, path_like: &str) -> Result<Vec<(String, String, String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT r.from_event, r.to_event, r.kind, r.level
             FROM event_relations r
             JOIN events e ON e.event_id = r.to_event
             WHERE json_extract(e.payload, '$.path') = ?1
             ORDER BY r.relation_id DESC LIMIT 200",
        )?;
        let rows = st
            .query_map([path_like], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count(&self, table: &str) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM {table}");
        Ok(self.conn.query_row(&sql, [], |r| r.get(0))?)
    }

    // ------------------------------------------------------------- retention

    /// Delete events older than `cutoff_ns`, in bounded batches so a sweep never
    /// holds the write lock for the whole deletion.
    pub fn trim_by_time(&self, cutoff_ns: i64, batch: usize) -> Result<usize> {
        let mut total = 0usize;
        loop {
            let n = self.conn.execute(
                "DELETE FROM events WHERE event_id IN (
                    SELECT event_id FROM events WHERE ts_mono_ns < ?1 LIMIT ?2)",
                rusqlite::params![cutoff_ns, batch as i64],
            )?;
            total += n;
            if n < batch {
                return Ok(total);
            }
        }
    }

    /// Delete the oldest rows until the table is at most `keep` rows.
    pub fn trim_by_size(&self, keep: i64, batch: usize) -> Result<usize> {
        let mut removed = 0usize;
        loop {
            let n: i64 = self
                .conn
                .query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
            if n <= keep {
                return Ok(removed);
            }
            let to_drop = (n - keep).min(batch as i64);
            let r = self.conn.execute(
                "DELETE FROM events WHERE event_id IN (
                    SELECT event_id FROM events ORDER BY ts_mono_ns ASC LIMIT ?1)",
                [to_drop],
            )?;
            removed += r;
            if r == 0 {
                return Ok(removed);
            }
        }
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    pub fn incremental_vacuum(&self, pages: i64) -> Result<()> {
        // Requires auto_vacuum=INCREMENTAL, which must be set before the first
        // table is created; measured and reported either way.
        let mode: i64 = self
            .conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
        if mode == 2 {
            self.conn
                .execute_batch(&format!("PRAGMA incremental_vacuum({pages})"))?;
        }
        Ok(())
    }

    pub fn journal_mode(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))?)
    }

    pub fn synchronous(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))?)
    }

    pub fn busy_timeout(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))?)
    }

    pub fn file_size(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

pub struct EventRow<'a> {
    pub event_id: &'a str,
    pub schema_version: i64,
    pub source: &'a str,
    pub session_id: Option<&'a str>,
    pub process_row_id: Option<i64>,
    pub sequence: i64,
    pub ts_mono_ns: i64,
    pub ts_wall: &'a str,
    pub type_: &'a str,
    pub type_class: &'a str,
    pub payload: &'a str,
    pub evidence_status: &'a str,
    pub dedup_group: Option<&'a str>,
    pub duplicate_of: Option<&'a str>,
    pub redaction_digest: Option<&'a str>,
    pub ingested_mono_ns: i64,
}

fn parse_version(file_name: &str) -> Option<i64> {
    let stem = file_name.strip_suffix(".sql")?;
    let head = stem.split('_').next()?;
    head.parse::<i64>().ok()
}

/// rusqlite helper: `query_row` that tolerates "no rows" without an error type
/// dance at every call site.
trait OptionalFirst<T> {
    fn optional_first(self) -> Result<Option<T>>;
}

impl<T> OptionalFirst<T> for rusqlite::Result<T> {
    fn optional_first(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(name: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp/li-phase0-h");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
        p
    }

    fn migrations() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
    }

    #[test]
    fn migrate_is_idempotent() {
        let p = tmp_db("t_idem.db");
        let mut s = Store::open(&p, DbConfig::default()).unwrap();
        let first = s.migrate(migrations(), None).unwrap();
        assert_eq!(first, vec![1, 2]);
        let second = s.migrate(migrations(), None).unwrap();
        assert!(second.is_empty());
        assert_eq!(s.schema_version().unwrap(), 2);
    }

    #[test]
    fn foreign_keys_are_on() {
        let p = tmp_db("t_fk.db");
        let mut s = Store::open(&p, DbConfig::default()).unwrap();
        s.migrate(migrations(), None).unwrap();
        let bad = s.conn.execute(
            "INSERT INTO events (event_id, schema_version, source, session_id, sequence,
                ts_mono_ns, ts_wall, type, type_class, payload, evidence_status, ingested_mono_ns)
             VALUES ('x', 1, 'proc', 'no-such-session', 0, 0, 'now', 'T', 'other', '{}', 'observed', 0)",
            [],
        );
        assert!(bad.is_err(), "FK must reject unknown session");
    }

    #[test]
    fn caused_by_is_rejected_by_the_schema() {
        let p = tmp_db("t_kind.db");
        let mut s = Store::open(&p, DbConfig::default()).unwrap();
        s.migrate(migrations(), None).unwrap();
        s.upsert_session("s1", "bash", None, "now", 0, None)
            .unwrap();
        for id in ["e1", "e2"] {
            s.insert_event(&EventRow {
                event_id: id,
                schema_version: 1,
                source: "fs",
                session_id: Some("s1"),
                process_row_id: None,
                sequence: 0,
                ts_mono_ns: 1,
                ts_wall: "now",
                type_: "FileModified",
                type_class: "fs.modify",
                payload: "{}",
                evidence_status: "observed",
                dedup_group: None,
                duplicate_of: None,
                redaction_digest: None,
                ingested_mono_ns: 0,
            })
            .unwrap();
        }
        let r = s.insert_relation("e1", "e2", "caused_by", "Confirmed", "R3");
        assert!(r.is_err(), "CHECK must reject caused_by");
        let ok = s.insert_relation("e1", "e2", "modified_during", "Weak", "R6");
        assert!(ok.is_ok());
    }
}
