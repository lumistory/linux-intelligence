//! Task item 5: many threads submitting at once, plus the two behaviour claims
//! the single-writer design is for — bounded queue drops are counted, and the
//! watermark is real delivery proof.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{event, fresh_dir};
use li_journal::write::Write;
use li_journal::{Accepted, Config, Journal};

/// 8 threads × 500 events through one writer: total correct, no duplicate
/// `event_id`, and every `sequence` distinct within its session.
#[test]
fn eight_threads_submit_into_one_writer() {
    let dir = fresh_dir("t-concurrent");
    let j = Arc::new(common::open_migrated(&dir, Config::default()));
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 500;

    for t in 0..THREADS {
        j.submit(common::session(&format!("s{t}")));
    }
    j.flush().unwrap();

    let accepted = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let j = Arc::clone(&j);
        let accepted = Arc::clone(&accepted);
        let dropped = Arc::clone(&dropped);
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                let id = format!("t{t}-e{i:04}");
                let ev = event(
                    &id,
                    &format!("s{t}"),
                    i,
                    "CommandFinished",
                    serde_json::json!({"command": format!("echo {t} {i}")}),
                );
                match j.event(&ev).expect("event") {
                    (Accepted::Queued, _) => {
                        accepted.fetch_add(1, Ordering::Relaxed);
                    }
                    (Accepted::Dropped, _) => {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    let w = j.flush().expect("flush");

    let accepted = accepted.load(Ordering::Relaxed);
    let dropped = dropped.load(Ordering::Relaxed);
    assert_eq!(
        accepted + dropped,
        THREADS * PER_THREAD,
        "every submission must be classified as queued or dropped"
    );
    assert_eq!(
        w.dropped, dropped,
        "the watermark must agree with what the callers were told"
    );
    assert_eq!(w.duplicate, 0, "no id was resent, so nothing may dedup");

    let reader = j.reader().expect("reader");
    assert_eq!(
        reader.count("events").unwrap(),
        accepted as i64,
        "stored rows must equal accepted submissions (dropped {} )",
        dropped
    );
    assert_eq!(reader.integrity_check().unwrap(), "ok");

    // `event_id` is UNIQUE, so a duplicate is impossible by construction; this
    // asserts the *count* agrees, which is what would break if the writer ever
    // lost a row silently.
    let distinct: i64 = reader
        .connection()
        .query_row("SELECT count(DISTINCT event_id) FROM events", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(distinct, reader.count("events").unwrap());

    // Every session's producer sequence is complete and hole-free.
    for t in 0..THREADS {
        let session = format!("s{t}");
        let gaps = reader.sequence_gaps(&session).unwrap();
        if dropped == 0 {
            assert!(gaps.is_empty(), "session {session} has holes: {gaps:?}");
            let n = reader.events_of_session(&session, 10_000).unwrap();
            assert_eq!(n.len() as u64, PER_THREAD);
            let seqs: Vec<i64> = n.iter().map(|(_, s)| *s).collect();
            let mut sorted = seqs.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), seqs.len(), "duplicate sequence in {session}");
        }
    }
}

/// A deliberately tiny queue must drop, **count**, and never block or grow.
#[test]
fn a_full_queue_drops_and_counts_instead_of_blocking() {
    let dir = fresh_dir("t-queue-drop");
    let config = Config {
        // One slot: the writer cannot possibly keep up with 200 submissions.
        queue_depth: 1,
        write_batch: 1,
        ..Config::default()
    };
    let j = common::open_migrated(&dir, config);
    j.submit(common::session("s1"));
    j.flush().unwrap();

    let mut dropped = 0;
    for i in 0..200u64 {
        match j
            .event(&event(
                &format!("q{i}"),
                "s1",
                i,
                "CommandFinished",
                serde_json::json!({}),
            ))
            .unwrap()
        {
            (Accepted::Dropped, _) => dropped += 1,
            (Accepted::Queued, _) => {}
        }
    }
    let w = j.flush().unwrap();
    assert!(dropped > 0, "a one-slot queue must drop under this load");
    assert_eq!(
        w.dropped, dropped,
        "the count must be reported, not swallowed (C1)"
    );
    let reader = j.reader().unwrap();
    assert_eq!(
        reader.count("events").unwrap() + dropped as i64,
        200,
        "dropped + stored must account for every submission"
    );
    assert_eq!(reader.integrity_check().unwrap(), "ok");
}

/// The watermark is monotone and is what "everything before this is durable"
/// means (C2: a per-event ack is not delivery proof).
#[test]
fn the_watermark_is_monotone_across_flushes() {
    let dir = fresh_dir("t-watermark");
    let j = common::open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    let mut previous = 0;
    for batch in 0..5u64 {
        for i in 0..10u64 {
            let n = batch * 10 + i;
            j.event(&event(
                &format!("w{n}"),
                "s1",
                n,
                "CommandFinished",
                serde_json::json!({}),
            ))
            .unwrap();
        }
        let w = j.flush().unwrap();
        assert!(
            w.ingest_seq >= previous,
            "watermark went backwards: {previous} -> {}",
            w.ingest_seq
        );
        // `written` counts every row of every kind, so it is the session plus the
        // events; the event count has to be compared against the events alone.
        assert_eq!(
            w.written,
            10 * (batch + 1) + 1,
            "watermark {w:?} last_error={:?}",
            j.last_error()
        );
        // Delivery proof: every event the watermark accounts for is queryable
        // *now*, before the next flush.
        let reader = j.reader().unwrap();
        assert_eq!(reader.count("events").unwrap(), 10 * (batch + 1) as i64);
        assert_eq!(reader.count("sessions").unwrap(), 1);
        previous = w.ingest_seq;
    }
}

/// Two journals do **not** run two writers without serialising: the second one
/// waits on `busy_timeout` instead of failing with `database is locked`.
///
/// This is the measurement behind `BUSY_TIMEOUT_MS = 5000` (0-H §7.2, where
/// `busy_timeout = 0` made writer B die outright).
#[test]
fn two_writers_serialise_rather_than_failing() {
    let dir = fresh_dir("t-two-writers");
    let db = dir.join("journal.db");
    let a = Arc::new(common::open_migrated(&dir, Config::default()));
    let b = Arc::new(Journal::open(&db, Config::default()).unwrap());

    let mut handles = Vec::new();
    for (name, j) in [("a", Arc::clone(&a)), ("b", Arc::clone(&b))] {
        handles.push(std::thread::spawn(move || {
            j.submit(common::session(&format!("s-{name}")));
            for i in 0..200u64 {
                j.event(&event(
                    &format!("{name}{i}"),
                    &format!("s-{name}"),
                    i,
                    "CommandFinished",
                    serde_json::json!({}),
                ))
                .unwrap();
            }
            j.flush().unwrap();
        }));
    }
    for h in handles {
        h.join()
            .expect("no writer may fail with `database is locked`");
    }
    let reader = a.reader().unwrap();
    assert_eq!(reader.count("events").unwrap(), 400);
    assert_eq!(reader.integrity_check().unwrap(), "ok");
}

/// A resend under the same `event_id` is deduplicated, not an error, and not a
/// second row (C3 / 0-C T7: 21/21 resends deduplicated on `event_id`).
#[test]
fn a_resent_event_id_is_deduplicated_and_counted() {
    let dir = fresh_dir("t-dedup");
    let j = common::open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    let ev = event("same", "s1", 0, "CommandFinished", serde_json::json!({}));
    j.event(&ev).unwrap();
    j.event(&ev).unwrap();
    j.event(&ev).unwrap();
    let w = j.flush().unwrap();
    assert_eq!(w.duplicate, 2, "two resends must be counted as duplicates");
    assert_eq!(w.failed, 0, "a resend is not a failure");
    assert_eq!(j.reader().unwrap().count("events").unwrap(), 1);
}

/// A `Write` row that names an event the journal does not have must not be
/// silently accepted: the relation is refused and counted.
#[test]
fn a_relation_to_a_missing_event_is_refused() {
    let dir = fresh_dir("t-relation-fk");
    let j = common::open_migrated(&dir, Config::default());
    j.submit(common::session("s1"));
    j.event(&event(
        "a",
        "s1",
        0,
        "CommandStarted",
        serde_json::json!({}),
    ))
    .unwrap();
    j.flush().unwrap();
    j.relate(
        "a",
        "does-not-exist",
        "observed_within",
        li_proto::CredibilityLevel::Weak,
        "R6",
    );
    let w = j.flush().unwrap();
    assert_eq!(w.failed, 1);
    assert!(
        j.last_error().unwrap().contains("FOREIGN KEY"),
        "reason: {:?}",
        j.last_error()
    );
    assert_eq!(j.reader().unwrap().count("event_relations").unwrap(), 0);
}

/// `Write` is `Send`, which is the whole point: a `li-daemon` task hands rows to
/// the writer from any thread. Compile-time claim, asserted so it cannot regress.
#[test]
fn write_rows_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Write>();
    assert_send::<li_journal::write::EventRow>();
    assert_send::<li_journal::write::Session>();
}
