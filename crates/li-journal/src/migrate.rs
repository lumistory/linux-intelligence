//! Forward-only migrations with a checksum ledger.
//!
//! Shape is 0-H §7.1, which is the only version of this that was run against a
//! populated database (5000 rows, v1 -> v2, zero loss):
//!
//! * files are `NNNN_description.sql`, applied in version order, upward only;
//! * the body is checksummed and the checksum recorded, so an already-applied
//!   file that was edited later is **detected** — never silently re-applied;
//! * DDL and its ledger row commit in one transaction, so a migration is either
//!   fully applied and recorded or not applied at all.

use rusqlite::Connection;

use crate::clock;
use crate::error::{JournalError, Result};

/// Migrations embedded in the binary.
///
/// Embedded, not read from disk: a daemon started from any working directory
/// must not be able to open a v1-only journal because `migrations/` was not in
/// `$PWD`. The consequence is that adding a migration requires a rebuild, which
/// is the correct trade for a component whose whole job is local durability.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "init",
        body: include_str!("../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "add_redaction_ledger",
        body: include_str!("../migrations/0002_add_redaction_ledger.sql"),
    },
];

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub body: &'static str,
}

impl Migration {
    pub fn checksum(&self) -> String {
        clock::fnv1a_hex(self.body.as_bytes())
    }
}

/// The newest version this build knows.
pub fn known_version() -> i64 {
    MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0)
}

/// Version recorded in the database. `0` means "no ledger yet", which is the
/// state of a file that migration 0001 has not touched.
pub fn current_version(conn: &Connection) -> Result<i64> {
    let has_ledger: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |r| r.get(0),
    )?;
    if has_ledger == 0 {
        return Ok(0);
    }
    let v: Option<i64> =
        conn.query_row("SELECT max(version) FROM schema_version", [], |r| r.get(0))?;
    Ok(v.unwrap_or(0))
}

/// Applies every migration with `version <= target` (all of them when `None`).
///
/// Returns the versions applied *now*, in order; an already-current database
/// returns an empty vector after verifying checksums, so "nothing happened" and
/// "everything happened" are distinguishable without a second call.
pub fn migrate(conn: &mut Connection, target: Option<i64>) -> Result<Vec<i64>> {
    let known = known_version();
    let mut current = current_version(conn)?;
    if let Some(t) = target
        && t > known
    {
        return Err(JournalError::NotRepresentable {
            field: "migration target beyond this build's known version",
        });
    }
    let target = target.unwrap_or(known);
    if current > target {
        return Err(JournalError::SchemaAhead {
            found: current,
            known: target,
        });
    }

    // Verify first, apply second: a drifted file must fail before any DDL runs.
    verify_applied(conn, current)?;

    let mut applied = Vec::new();
    for m in MIGRATIONS.iter().filter(|m| m.version <= target) {
        if m.version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(m.body)?;
        tx.execute(
            "INSERT INTO schema_version (version, applied_at, description, checksum)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                m.version,
                clock::wall_now_rfc3339_ms(),
                m.name,
                m.checksum()
            ],
        )?;
        tx.commit()?;
        current = m.version;
        applied.push(m.version);
    }
    Ok(applied)
}

/// Recomputes the checksum of every recorded migration. A mismatch is a broken
/// series, not something to repair at runtime.
fn verify_applied(conn: &Connection, current: i64) -> Result<()> {
    if current == 0 {
        return Ok(());
    }
    for m in MIGRATIONS.iter().filter(|m| m.version <= current) {
        let recorded: Option<String> = conn
            .query_row(
                "SELECT checksum FROM schema_version WHERE version = ?1",
                [m.version],
                |r| r.get(0),
            )
            .ok();
        if let Some(recorded) = recorded
            && recorded != m.checksum()
        {
            return Err(JournalError::MigrationDrift {
                file: format!("{:04}_{}.sql", m.version, m.name),
            });
        }
    }
    Ok(())
}

/// Applied (version, name, applied_at, checksum), oldest first.
pub fn ledger(conn: &Connection) -> Result<Vec<(i64, String, String, String)>> {
    if current_version(conn)? == 0 {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare(
        "SELECT version, description, applied_at, checksum FROM schema_version ORDER BY version",
    )?;
    let rows = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
