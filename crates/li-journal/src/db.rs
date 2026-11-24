//! Connection configuration. Every pragma here is a Phase 0 measurement, and
//! each one is re-asserted on every connection that opens the file.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::error::{JournalError, Result};

/// Write-contention backoff, in milliseconds.
///
/// 5000 is taken from 0-H §7.2, which is the only place it was measured, and it
/// is not a round number chosen for looks:
///
/// * With `busy_timeout = 0`, two concurrent writers did **not** serialise —
///   writer A committed 316 922 events and writer B died with
///   `database is locked`. A hard failure of a normal overlap is the outcome
///   this value exists to prevent.
/// * With `busy_timeout = 5000`, the same two writers both succeeded
///   (30 093 + 320 827 commits, 0 errors) and four concurrent readers recorded
///   0 busy retries and p99 367–398 us.
///
/// Why 5 s and not more: the longest *legitimate* write transaction measured in
/// this project is the 100 000-row bulk insert at 516 ms (0-H §7.4), so 5 s is
/// ~10x the worst case. Waiting longer than that does not serialise a slow
/// writer, it hides a wedged one — and this crate's write path is a queue whose
/// depth is bounded, so a caller blocked in `flush` would turn "the writer is
/// stuck" into "the daemon hangs", which is the failure mode C1 forbids.
///
/// Note: readers never need this under WAL (0-H measured 0 busy retries with the
/// timeout at 0). It is set on every connection anyway, because a reader that
/// later gains a write path must not inherit a 0 default — SQLite's default is 0.
pub const BUSY_TIMEOUT_MS: u64 = 5_000;

/// Statement cache pages per connection. 64 MiB, as in 0-H, which measured the
/// 100k-row query shapes inside 10 ms with it.
const CACHE_SIZE_KIB: i64 = -65_536;

#[derive(Debug, Clone)]
pub struct Config {
    pub busy_timeout_ms: u64,
    /// `false` = `synchronous=NORMAL`, which is what 0-H measured the `kill -9`
    /// result under (§7.3). `FULL` adds an fsync per commit and moves the crash
    /// boundary; 0-H §8.4 explicitly leaves that decision to measurement, so the
    /// knob is exposed rather than assumed.
    pub synchronous_full: bool,
    /// Events per transaction in the writer. 0-H inserted 100 000 rows in one
    /// transaction at 193 432 rows/s; per-event commits are two orders slower.
    /// 128 keeps one transaction under ~1 ms of work.
    pub write_batch: usize,
    /// Bounded queue in front of the writer. On overflow the event is dropped
    /// **and counted** (C1: a bounded queue drops and counts; `sequence` is the
    /// producer's, so the gap it leaves is detectable downstream).
    pub queue_depth: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            busy_timeout_ms: BUSY_TIMEOUT_MS,
            synchronous_full: false,
            write_batch: 128,
            queue_depth: 1024,
        }
    }
}

/// Opens the database file and applies every configured pragma.
///
/// `journal_mode = WAL` is *verified*, not requested: a database created by
/// another tool (or restored from a backup taken with `-journal`) would
/// otherwise run in rollback-journal mode and change both the concurrency story
/// and the `kill -9` story without anyone noticing.
pub fn open(path: &Path, config: &Config) -> Result<Connection> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    configure(&conn, config)?;
    Ok(conn)
}

fn configure(conn: &Connection, config: &Config) -> Result<()> {
    // Explicit, always. SQLite's default for both of these is the *other*
    // value, and a forgotten `foreign_keys=ON` "passes" while orphaning rows.
    conn.busy_timeout(std::time::Duration::from_millis(config.busy_timeout_ms))?;
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(JournalError::Integrity(format!(
            "journal_mode is {mode}, not wal"
        )));
    }
    apply_per_connection_pragmas(conn, config)?;
    Ok(())
}

/// The pragmas that are **per connection**, not per file.
///
/// This distinction is the reason this function exists at all: `journal_mode` is
/// stored in the database header, but `synchronous`, `foreign_keys`,
/// `busy_timeout` and `cache_size` are per-connection and default to values this
/// project does not want (`synchronous` defaults to FULL, `foreign_keys` to OFF,
/// `busy_timeout` to 0). A second connection that skips them — a reader, say —
/// silently runs with different durability and locking settings, which is exactly
/// the class of bug PHASE0-REPORT §6 warns about ("a prototype that forgets it
/// passes while orphaning rows").
///
/// Found by `the_configured_pragmas_are_actually_in_effect`: the reader reported
/// `synchronous=2` (FULL) while the writer was on NORMAL.
pub(crate) fn apply_per_connection_pragmas(conn: &Connection, config: &Config) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(config.busy_timeout_ms))?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(if config.synchronous_full {
        "PRAGMA synchronous=FULL;"
    } else {
        "PRAGMA synchronous=NORMAL;"
    })?;
    conn.execute_batch(&format!("PRAGMA cache_size={CACHE_SIZE_KIB};"))?;
    conn.execute_batch("PRAGMA wal_autocheckpoint=1000;")?;
    Ok(())
}

/// The database path the spec names: `~/.local/state/linux-intelligence/journal.db`.
///
/// Returns `None` rather than guessing when `HOME` is unset or relative — a
/// journal written to the wrong directory is worse than an error.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return None;
    }
    Some(
        home.join(".local")
            .join("state")
            .join("linux-intelligence")
            .join("journal.db"),
    )
}

/// `fsync`-free check of what the pragmas actually are, for tests and `li doctor`.
///
/// Values come back as plain text (`wal`, `5000`), not as a `Debug` rendering of a
/// SQLite value: this is meant to be printable in a doctor report and compared
/// against a documented number.
pub fn effective_settings(conn: &Connection) -> Result<Vec<(String, String)>> {
    use rusqlite::types::Value;

    let mut out = Vec::new();
    for pragma in [
        "journal_mode",
        "synchronous",
        "foreign_keys",
        "busy_timeout",
        "wal_autocheckpoint",
    ] {
        let v: Value = conn.query_row(&format!("PRAGMA {pragma}"), [], |r| r.get(0))?;
        let text = match v {
            Value::Integer(i) => i.to_string(),
            Value::Real(f) => f.to_string(),
            Value::Text(t) => t,
            Value::Blob(_) => "<blob>".to_string(),
            Value::Null => "<null>".to_string(),
        };
        out.push((pragma.to_string(), text));
    }
    Ok(out)
}
