//! Test fixture: a writer that runs until it is killed.
//!
//! `tests/crash.rs` spawns this, lets it commit for a while, then sends a real
//! `SIGKILL`. It exists as a separate binary because an in-process thread cannot
//! be killed — `SIGKILL` takes down the whole process, and the point of the test
//! is that the *database* survives losing its writer mid-batch, not that a thread
//! returns an error.
//!
//! Protocol: prints one line per committed batch to stdout, `committed <n>
//! <last_event_id>`; the parent reads the last line to learn what was durable
//! before the kill.
//!
//! Not a product binary. It is `#[doc(hidden)]`-equivalent by being a `bin` under
//! a crate that ships a library; `cargo build --workspace` still compiles it,
//! which is intentional: a fixture that only compiles under `cfg(test)` can rot.

use li_journal::Journal;
use li_journal::db::Config;
use li_journal::write::{EventRow, Session, Write};
use std::io::Write as _;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: li-journal-crash-probe <db-path>");
    let mut journal = Journal::open(std::path::Path::new(&path), Config::default())?;
    journal.migrate()?;

    journal.submit(Write::Session(Session {
        session_id: "crash".into(),
        parent_session: None,
        adapter: "bash".into(),
        adapter_version: None,
        shell_pid: None,
        started_wall: "2026-11-24T00:00:00.000Z".into(),
        started_mono_ns: 1,
        cwd: None,
        host: None,
        boot_id: None,
    }));

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for i in 0u64.. {
        let id = format!("crash-{i:08}");
        journal.submit(Write::Event(EventRow {
            event_id: id.clone(),
            schema_version: 1,
            source: "shell.bash".into(),
            session_id: Some("crash".into()),
            process_row_id: None,
            sequence: i as i64,
            ts_mono_ns: i as i64,
            ts_wall: "2026-11-24T00:00:00.000Z".into(),
            type_: "CommandFinished".into(),
            type_class: "proc.end".into(),
            payload: format!(r#"{{"command":"echo {i}"}}"#),
            evidence_status: "declared".into(),
            dedup_group: None,
            duplicate_of: None,
            redaction_rules: None,
            redaction_hits: None,
            redaction_digest: None,
            fp_type_class: None,
            fp_subject: None,
            fp_bucket_ns: None,
            ingested_mono_ns: i as i64,
        }));
        // Flush every 32 rows and report: the parent then knows a durable prefix
        // exists, and can require that the prefix is intact afterwards.
        if i % 32 == 31 {
            journal.flush()?;
            // Report the *event* count read back from the file, not the writer's
            // `written` counter: that counter also counts the session row, and a
            // fixture that reports a number the parent cannot check against
            // `count(*) FROM events` invites a test that passes for the wrong
            // reason.
            let events = journal.reader()?.count("events")?;
            writeln!(out, "committed {events} {id}")?;
            out.flush()?;
        }
    }
    Ok(())
}
