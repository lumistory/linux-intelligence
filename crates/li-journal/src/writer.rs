//! **The single writer.** One thread owns the only write connection.
//!
//! # Why this shape
//!
//! Spec §3 is explicit: the measured ingest bottleneck is the journal flush, not
//! the socket (0-C), so SQLite gets exactly one writer thread and everything
//! else feeds it. The API is therefore shaped as *hand the row to a writer*, not
//! *open a connection and INSERT*:
//!
//! ```text
//!   N producers ──submit()──▶ bounded queue ──▶ 1 writer thread ──▶ 1 connection
//!   (daemon tasks,            (drop + count)     (batch, 1 txn)      (WAL)
//!    test threads)                                                     │
//!                                                          flush() ◀───┘
//!                                                          (watermark)
//! ```
//!
//! Three consequences that the daemon depends on, each deliberate:
//!
//! 1. **`submit` does not block on the disk.** It is a channel send. A slow
//!    commit cannot stall a shell hook's ingest path, which is the property
//!    `li-daemon` needs in order to keep its own ingest bounded and honest.
//! 2. **The queue is bounded and drops are counted** (C1). `submit` returns
//!    `Accepted::Dropped` rather than blocking or growing without limit, so a
//!    wedged writer shows up as a number the daemon can report, not as memory
//!    growth.
//! 3. **Delivery is proven by the watermark, not by a per-row ack** (C2, 0-C
//!    T1: 12.0 % of acks were dropped under a 50-client burst). `flush()` returns
//!    the highest committed `ingest_seq`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rusqlite::Connection;

use crate::db::Config;
use crate::error::{JournalError, Result};
use crate::write::{Watermark, Write};

/// Whether the writer took the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    Queued,
    /// The queue was full. The row is **not** stored, and `dropped` in the next
    /// watermark reflects it.
    Dropped,
}

enum Command {
    Row(Box<Write>),
    Flush(SyncSender<Result<Watermark>>),
    Stop,
}

pub(crate) struct Shared {
    ingest_seq: AtomicU64,
    written: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
    duplicate: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            ingest_seq: AtomicU64::new(0),
            written: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            duplicate: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    fn watermark(&self) -> Watermark {
        Watermark {
            ingest_seq: self.ingest_seq.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            duplicate: self.duplicate.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct Writer {
    tx: SyncSender<Command>,
    pub(crate) shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl Writer {
    pub(crate) fn spawn(conn: Connection, config: Config) -> Writer {
        let (tx, rx) = sync_channel::<Command>(config.queue_depth.max(1));
        let shared = Arc::new(Shared::new());
        let thread_shared = Arc::clone(&shared);
        let batch = config.write_batch.max(1);
        let handle = std::thread::Builder::new()
            // A name, because a `kill -9` transcript that names the thread is a
            // transcript that can point at the writer.
            .name("li-journal-writer".to_string())
            .spawn(move || run(conn, &rx, &thread_shared, batch))
            .expect("spawning the journal writer thread");
        Writer {
            tx,
            shared,
            handle: Some(handle),
        }
    }

    pub(crate) fn submit(&self, row: Write) -> Accepted {
        match self.tx.try_send(Command::Row(Box::new(row))) {
            Ok(()) => Accepted::Queued,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                Accepted::Dropped
            }
            // The writer thread is gone; count it as a drop rather than a lie.
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                Accepted::Dropped
            }
        }
    }

    /// Blocks until every row queued *before* this call is committed.
    pub(crate) fn flush(&self) -> Result<Watermark> {
        let (tx, rx) = sync_channel::<Result<Watermark>>(1);
        self.tx
            .send(Command::Flush(tx))
            .map_err(|_| JournalError::WriterGone)?;
        rx.recv().map_err(|_| JournalError::WriterGone)?
    }

    /// The most recent row-level failure, for a caller that wants to say *why*
    /// something was refused. Never contains a payload.
    pub(crate) fn last_error(&self) -> Option<String> {
        self.shared.last_error.lock().ok().and_then(|g| g.clone())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(conn: Connection, rx: &Receiver<Command>, shared: &Shared, batch: usize) {
    let mut pending: Vec<Write> = Vec::with_capacity(batch);
    loop {
        // Block for one command, then drain whatever else is already queued: one
        // transaction per batch, not per event, is the difference between 193 432
        // rows/s and a commit per row (0-H §7.4).
        let first = match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match first {
            None => continue,
            Some(Command::Stop) => break,
            Some(Command::Flush(reply)) => {
                flush_pending(&conn, &mut pending, shared);
                let _ = reply.send(Ok(shared.watermark()));
                continue;
            }
            Some(Command::Row(w)) => pending.push(*w),
        }
        while pending.len() < batch {
            match rx.try_recv() {
                Ok(Command::Row(w)) => pending.push(*w),
                Ok(Command::Flush(reply)) => {
                    flush_pending(&conn, &mut pending, shared);
                    let _ = reply.send(Ok(shared.watermark()));
                }
                Ok(Command::Stop) => {
                    flush_pending(&conn, &mut pending, shared);
                    return;
                }
                Err(_) => break,
            }
        }
        flush_pending(&conn, &mut pending, shared);
    }
    flush_pending(&conn, &mut pending, shared);
}

fn note_error(shared: &Shared, e: JournalError, kind: &'static str) {
    shared.failed.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut slot) = shared.last_error.lock() {
        *slot = Some(format!("{kind}: {e}"));
    }
}

/// Commits the pending rows, isolating any row the database refuses.
///
/// # Why a refused row does not fail the caller
///
/// A `flush()` reports *how much is durable and how much was refused*
/// ([`Watermark::failed`]), it does not abort: an FK violation on one event is a
/// fact about that event, not a broken journal. Returning `Err` from `flush` for
/// it would also invite a caller to retry the whole batch, which re-offends.
///
/// The isolation is the part that matters for correctness. One transaction per
/// batch is what makes ingest fast (0-H: 193 432 rows/s), but a batch that fails
/// as a unit would discard 127 innocent rows because one had a dangling
/// `session_id`. So: try the batch, and on failure replay it row by row. Rows that
/// still fail are counted in `failed` and their reason kept in `last_error`, and
/// the rest are committed.
fn flush_pending(conn: &Connection, pending: &mut Vec<Write>, shared: &Shared) {
    if pending.is_empty() {
        return;
    }
    if commit(conn, pending, shared).is_ok() {
        pending.clear();
        return;
    }
    // Rolled back already. Replay individually.
    let rows = std::mem::take(pending);
    for row in rows {
        let kind = row.kind();
        if let Err(e) = commit(conn, std::slice::from_ref(&row), shared) {
            note_error(shared, e, kind);
        }
    }
}

/// What a row contributed, decided **before** the commit and published after it.
///
/// The counters are deliberately not incremented inside `apply`: a row that is
/// written and then rolled back must not appear in the watermark, or the
/// watermark stops being delivery proof (C2). This was a real bug — a rolled-back
/// batch left `written` one too high, which is exactly the lie the design exists
/// to prevent.
#[derive(Default)]
struct Tallies {
    written: u64,
    duplicate: u64,
}

impl Tallies {
    fn publish(self, shared: &Shared) {
        if self.written > 0 {
            shared.written.fetch_add(self.written, Ordering::Relaxed);
        }
        if self.duplicate > 0 {
            shared
                .duplicate
                .fetch_add(self.duplicate, Ordering::Relaxed);
        }
    }
}

/// One transaction for the whole slice: either all of it is durable or none is.
fn commit(conn: &Connection, rows: &[Write], shared: &Shared) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    // `BEGIN IMMEDIATE` is what rusqlite's `Transaction::new` does not do by
    // default: taking the write lock at BEGIN rather than at the first INSERT
    // turns a mid-transaction `SQLITE_BUSY` into a plain wait bounded by
    // `busy_timeout`.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = apply_all(conn, rows);
    match outcome {
        Ok(tallies) => {
            conn.execute_batch("COMMIT")?;
            // Only now is the row durable, and only now may it count.
            tallies.publish(shared);
            // The watermark is the highest committed `ingest_seq` **in the file**,
            // not an independent counter: `max()` on an INTEGER PRIMARY KEY is an
            // O(1) index lookup, and reading it back from the database makes the
            // number true by construction rather than by bookkeeping.
            let max: Option<i64> =
                conn.query_row("SELECT max(ingest_seq) FROM events", [], |r| r.get(0))?;
            if let Some(v) = max {
                shared
                    .ingest_seq
                    .fetch_max(u64::try_from(v).unwrap_or(0), Ordering::Relaxed);
            }
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn apply_all(conn: &Connection, rows: &[Write]) -> Result<Tallies> {
    let mut tallies = Tallies::default();
    for row in rows {
        let outcome = apply(conn, row)?;
        tallies.written += 1;
        if outcome == RowOutcome::Duplicate {
            tallies.duplicate += 1;
        }
    }
    Ok(tallies)
}

/// Whether the row actually appeared, for the one kind where it can be refused
/// without being an error: a resent `event_id` (C3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowOutcome {
    Inserted,
    Duplicate,
}

fn apply(conn: &Connection, row: &Write) -> Result<RowOutcome> {
    match row {
        Write::Session(s) => {
            conn.prepare_cached(
                "INSERT INTO sessions
                   (session_id, parent_session, adapter, adapter_version, shell_pid,
                    started_wall, started_mono_ns, cwd, host, boot_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(session_id) DO NOTHING",
            )?
            .execute(rusqlite::params![
                s.session_id,
                s.parent_session,
                s.adapter,
                s.adapter_version,
                s.shell_pid,
                s.started_wall,
                s.started_mono_ns,
                s.cwd,
                s.host,
                s.boot_id,
            ])?;
            Ok(RowOutcome::Inserted)
        }
        Write::Process(p) => {
            conn.prepare_cached(
                "INSERT INTO processes
                   (pid, starttime, incarnation_seq, comm, ppid, session_id,
                    first_seen_mono_ns, ambiguous)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(pid, starttime, incarnation_seq) DO UPDATE SET
                    last_seen_mono_ns = excluded.first_seen_mono_ns,
                    -- sticky on purpose: 0-H §8.3.2 requires `ambiguous = 1` to
                    -- survive a restart, so a later clean observation may not
                    -- launder it back to 0
                    ambiguous = max(processes.ambiguous, excluded.ambiguous)",
            )?
            .execute(rusqlite::params![
                p.pid,
                p.starttime,
                p.incarnation_seq,
                p.comm,
                p.ppid,
                p.session_id,
                p.first_seen_mono_ns,
                i64::from(p.ambiguous),
            ])?;
            Ok(RowOutcome::Inserted)
        }
        Write::Event(e) => {
            let inserted = conn
                .prepare_cached(
                    "INSERT INTO events
                       (event_id, schema_version, source, session_id, process_row_id,
                        sequence, ts_mono_ns, ts_wall, type, type_class, payload,
                        evidence_status, dedup_group, duplicate_of, redaction_rules,
                        redaction_hits, redaction_digest, fp_type_class, fp_subject,
                        fp_bucket_ns, ingested_mono_ns)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,
                             ?16,?17,?18,?19,?20,?21)
                     ON CONFLICT(event_id) DO NOTHING",
                )?
                .execute(rusqlite::params![
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
                    e.redaction_rules,
                    e.redaction_hits,
                    e.redaction_digest,
                    e.fp_type_class,
                    e.fp_subject,
                    e.fp_bucket_ns,
                    e.ingested_mono_ns,
                ])?;
            if inserted == 0 {
                // A resend under the same `event_id`: the dedup that is allowed
                // to be free and exact (C3). Not an error.
                Ok(RowOutcome::Duplicate)
            } else {
                Ok(RowOutcome::Inserted)
            }
        }
        Write::Operation(o) => {
            conn.prepare_cached(
                "INSERT INTO operations
                   (operation_id, session_id, label, intent, state, started_mono_ns)
                 VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(operation_id) DO NOTHING",
            )?
            .execute(rusqlite::params![
                o.operation_id,
                o.session_id,
                o.label,
                o.intent,
                o.state,
                o.started_mono_ns,
            ])?;
            Ok(RowOutcome::Inserted)
        }
        Write::OperationEdge(x) => {
            conn.prepare_cached(
                "INSERT INTO event_operations
                   (event_id, operation_id, attached_by, level, attached_mono_ns)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(event_id, operation_id) DO UPDATE SET
                    level = excluded.level,
                    attached_by = excluded.attached_by",
            )?
            .execute(rusqlite::params![
                x.event_id,
                x.operation_id,
                x.attached_by,
                x.level,
                x.attached_mono_ns,
            ])?;
            Ok(RowOutcome::Inserted)
        }
        Write::Relation(r) => {
            conn.prepare_cached(
                "INSERT INTO event_relations
                   (from_event, to_event, kind, level, rule, inferred_at_mono_ns)
                 VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(from_event, to_event, kind) DO UPDATE SET
                    level = excluded.level,
                    rule = excluded.rule",
            )?
            .execute(rusqlite::params![
                r.from_event,
                r.to_event,
                r.kind,
                r.level,
                r.rule,
                r.inferred_at_mono_ns,
            ])?;
            Ok(RowOutcome::Inserted)
        }
        Write::Evidence(ev) => {
            conn.prepare_cached(
                "INSERT INTO evidence
                   (relation_id, source, source_class, evidence_status, independent,
                    confidence, detail)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?
            .execute(rusqlite::params![
                ev.relation_id,
                ev.source,
                ev.source_class,
                ev.evidence_status,
                i64::from(ev.independent),
                ev.confidence,
                ev.detail,
            ])?;
            Ok(RowOutcome::Inserted)
        }
    }
}
