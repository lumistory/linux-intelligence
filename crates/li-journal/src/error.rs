//! One error type, because every failure here ends the same way: the event is
//! *not* stored, and the caller must be able to say why without parsing text.

use std::fmt;

#[derive(Debug)]
pub enum JournalError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    /// A migration file was edited after it was applied. Not repairable by
    /// re-running: the series is the bug (0-H §7.1 verifies instead of
    /// re-applying).
    MigrationDrift {
        file: String,
    },
    /// The database was written by a newer build. Forward-only means this is a
    /// refusal, not a best-effort read.
    SchemaAhead {
        found: i64,
        known: i64,
    },
    /// A value the schema cannot hold (e.g. a `u64` beyond `i64::MAX`).
    NotRepresentable {
        field: &'static str,
    },
    /// The integrity check the caller asked for failed.
    Integrity(String),
    /// The writer thread is gone. The event was not stored.
    WriterGone,
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::Sqlite(e) => write!(f, "sqlite: {e}"),
            JournalError::Io(e) => write!(f, "io: {e}"),
            JournalError::MigrationDrift { file } => {
                write!(
                    f,
                    "migration {file} is already applied with a different checksum"
                )
            }
            JournalError::SchemaAhead { found, known } => {
                write!(
                    f,
                    "database is at v{found}, this build knows up to v{known}"
                )
            }
            JournalError::NotRepresentable { field } => {
                write!(f, "`{field}` does not fit the schema's integer range")
            }
            JournalError::Integrity(m) => write!(f, "integrity: {m}"),
            JournalError::WriterGone => write!(f, "writer thread is gone; event not stored"),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JournalError::Sqlite(e) => Some(e),
            JournalError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for JournalError {
    fn from(e: rusqlite::Error) -> Self {
        JournalError::Sqlite(e)
    }
}

impl From<std::io::Error> for JournalError {
    fn from(e: std::io::Error) -> Self {
        JournalError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, JournalError>;
