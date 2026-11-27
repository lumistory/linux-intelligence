use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use li_daemon::observers::fs::FsWatcher;
use li_daemon::observers::procfs::{ProcPoller, ProcStat, parse_stat, scan_proc_table};
use li_daemon::observers::{register_capabilities, system_observer_capabilities};
use li_journal::{Config, Journal};

fn tempdir(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("li-obs-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&p).unwrap();
    p
}

fn open_journal(path: &Path) -> Journal {
    let mut j = Journal::open(path, Config::default()).unwrap();
    j.migrate().unwrap();
    j
}

#[test]
fn system_observer_capabilities_published_verbatim_and_persisted() {
    let root = tempdir("caps");
    let j = open_journal(&root.join("journal.db"));
    register_capabilities(&j);
    j.flush().unwrap();

    let caps = system_observer_capabilities();
    assert_eq!(caps["profile"], "known-lossy");
    assert_eq!(
        caps["capabilities"]["file_writer_attribution"]["declaration"],
        false
    );
    assert_eq!(
        caps["capabilities"]["event_delivery_completeness"]["declaration"],
        "partial:IN_Q_OVERFLOW is data loss"
    );

    let r = j.reader().unwrap();
    let stored: String = r
        .connection()
        .query_row(
            "SELECT capabilities FROM adapter_registry WHERE adapter = 'system-observer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(parsed, caps);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn procfs_poller_handles_host_scan_and_marks_20_of_20_same_tick_collisions_ambiguous() {
    let root = tempdir("proc");
    let j = open_journal(&root.join("journal.db"));

    // Verify real /proc scan parses cleanly and stays fast (<50 ms even on debug builds).
    let t0 = Instant::now();
    let live = scan_proc_table();
    let elapsed = t0.elapsed();
    assert!(
        !live.is_empty(),
        "host /proc must contain running processes"
    );
    assert!(
        elapsed.as_millis() < 250,
        "host /proc scan unexpectedly slow: {elapsed:?}"
    );
    // Verify comm with spaces and parentheses parses accurately (P2-01).
    let parsed = parse_stat(
        "1234 (kworker/u16:1 (test)) I 2 0 0 0 -1 4194304 100 0 0 0 7 8 0 0 20 0 1 0 98765 123 456",
    )
    .unwrap();
    assert_eq!(parsed.comm, "kworker/u16:1 (test)");
    assert_eq!(parsed.starttime, 98765);

    let mut poller = ProcPoller::new(true);
    for trial in 0..20 {
        let mut snap = BTreeMap::new();
        snap.insert(
            4242,
            ProcStat {
                pid: 4242,
                comm: format!("worker-{trial}"),
                state: 'R',
                ppid: 1000,
                starttime: 555_555,
            },
        );
        let st = poller.apply_snapshot(&snap, &j, false);
        if trial > 0 {
            assert_eq!(st.same_tick_collisions, 1);
        }
        // Process disappears before next tick -> next trial reuses (4242, 555_555).
        let empty = BTreeMap::new();
        let gone = poller.apply_snapshot(&empty, &j, false);
        assert_eq!(gone.exited, 1);
    }

    j.flush().unwrap();
    let r = j.reader().unwrap();
    let (inc_count, ambig_count): (i64, i64) = r
        .connection()
        .query_row(
            "SELECT count(*), sum(ambiguous) FROM processes WHERE pid = 4242 AND starttime = 555555",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(inc_count, 20);
    assert_eq!(ambig_count, 19); // First sighting clean; all 19 reuses marked ambiguous=1.
    let _ = fs::remove_dir_all(root);
}

#[test]
fn inotify_watcher_auto_watches_subdirs_and_never_attributes_writer() {
    let root = tempdir("fs");
    let watch_root = root.join("watched");
    fs::create_dir_all(&watch_root).unwrap();
    let j = open_journal(&root.join("journal.db"));

    let mut watcher = FsWatcher::new(vec![watch_root.clone()]).unwrap();
    assert_eq!(watcher.watch_count(), 1);

    let file_a = watch_root.join("alpha.txt");
    let file_b = watch_root.join("beta.txt");
    fs::write(&file_a, "hello").unwrap();
    fs::rename(&file_a, &file_b).unwrap();

    // Create a new subdir, drain so auto-add fires, then write a file inside it.
    let sub = watch_root.join("sub");
    fs::create_dir(&sub).unwrap();
    let s1 = watcher.drain_now(&j).unwrap();
    assert!(s1.events_emitted >= 3);
    assert!(s1.watches_added >= 1);

    let sub_file = sub.join("nested.txt");
    fs::write(&sub_file, "inside").unwrap();
    let s2 = watcher.drain_now(&j).unwrap();
    assert!(s2.events_emitted >= 1);

    j.flush().unwrap();
    let r = j.reader().unwrap();
    let events = r.query_events(None, None, None, 50).unwrap();
    assert!(!events.is_empty());
    for ev in &events {
        assert_eq!(ev.evidence_status, "observed");
        assert!(
            ev.payload.get("pid").is_none(),
            "C4 forbids pid attribution"
        );
        assert!(
            ev.payload.get("uid").is_none(),
            "C4 forbids uid attribution"
        );
    }
    assert!(
        !r.events_touching_path(&sub_file.to_string_lossy(), 10)
            .unwrap()
            .is_empty()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn inotify_overflow_triggers_rescan_and_records_data_loss() {
    let root = tempdir("overflow");
    let watch_root = root.join("flood");
    fs::create_dir_all(&watch_root).unwrap();
    let j = open_journal(&root.join("journal.db"));

    let mut watcher = FsWatcher::new(vec![watch_root.clone()]).unwrap();

    // Exceed default /proc/sys/fs/inotify/max_queued_events (16384) before draining:
    // creating 18,500 files produces >= 37,000 events (CREATE + CLOSE_WRITE).
    for i in 0..18_500usize {
        let p = watch_root.join(format!("f{i}"));
        fs::File::create(&p).unwrap();
    }
    let late_dir = watch_root.join("created_during_overflow");
    fs::create_dir(&late_dir).unwrap();

    let stats = watcher.drain_now(&j).unwrap();
    assert!(
        stats.overflows >= 1,
        "expected IN_Q_OVERFLOW after 18,500 files without draining, got {stats:?}"
    );
    assert!(stats.rescans >= 1);

    j.flush().unwrap();
    let r = j.reader().unwrap();
    let overflows = r
        .query_events(None, None, Some("InotifyOverflow"), 10)
        .unwrap();
    assert!(!overflows.is_empty());
    assert_eq!(overflows[0].payload["data_loss"], true);
    assert_eq!(overflows[0].payload["reason"], "IN_Q_OVERFLOW");
    let _ = fs::remove_dir_all(root);
}
