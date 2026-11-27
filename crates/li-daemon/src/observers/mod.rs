//! Phase 2 system observers (`procfs` 200 ms poller & non-blocking `inotify`).
//!
//! Each observer runs in an isolated task with its own failure budget so a
//! read failure never stalls or takes down the shell UDS ingest path (spec §3).

pub mod fs;
pub mod procfs;

use li_journal::write::AdapterRecord;
use li_journal::{Journal, clock};

/// Verbatim Phase 2 capability boundary from `experiments/00-e-procfs-inotify/REPORT.md` §8.2.
///
/// Must stay `known-lossy`: never hide polling blind spots (<60 ms lifetimes),
/// same-tick PID collision windows, inotify `IN_Q_OVERFLOW` loss, or the
/// structural absence of `file -> writer` attribution.
pub fn system_observer_capabilities() -> serde_json::Value {
    serde_json::json!({
        "profile": "known-lossy",
        "poll_interval_ms": 200,
        "scan_depth": "Stat_default_Cmdline_on_create",
        "inotify_read_buffer_bytes": 65536,
        "capabilities": {
            "process_table": {
                "declaration": "true, observed",
                "fields": ["pid", "ppid", "comm", "state", "starttime", "cmdline"],
                "evidence": "0-E A8: 0 read failures on 504 processes"
            },
            "process_creation_below_60ms": {
                "declaration": "false at 200 ms",
                "evidence": "0-E A1b: 71.67 % lost at 60 ms, 94.17 % at 15 ms"
            },
            "process_exit_known_pid": {
                "declaration": "true (detected at next scan)",
                "evidence": "0-E A1 zombie control; A5 exit readability"
            },
            "process_identity": {
                "declaration": "(pid,starttime), partial:collision window <=1 tick (10 ms)",
                "evidence": "0-E A6b: 20/20 same-tick collisions"
            },
            "orphan_detection": {
                "declaration": "partial:do not assume ppid==1",
                "evidence": "0-E A7: reparented to systemd --user"
            },
            "file_basic_events": {
                "declaration": "true, observed",
                "kinds": ["create", "modify", "close", "delete", "attrib"],
                "evidence": "0-E B1"
            },
            "file_rename_same_fs": {
                "declaration": "true when both sides watched; partial otherwise",
                "evidence": "0-E B1: MOVED_FROM with no MOVED_TO into an unwatched dir"
            },
            "file_rename_cross_fs": {
                "declaration": "false — reported as create+write",
                "evidence": "0-E B2: move_pairs_seen 0"
            },
            "events_in_newly_created_subtrees": {
                "declaration": "partial:lost until watch is added",
                "evidence": "0-E B4: 0 events in newdir; 0 for immediate.txt even with auto-add"
            },
            "event_delivery_completeness": {
                "declaration": "partial:IN_Q_OVERFLOW is data loss",
                "evidence": "0-E B5: 16385 records delivered of >=50000 expected"
            },
            "file_writer_attribution": {
                "declaration": false,
                "evidence": "0-E B8, B10: no pid/uid in the ABI"
            },
            "watching_other_uid_paths": {
                "declaration": "false (EACCES)",
                "evidence": "0-E B6: errno 13"
            },
            "hidepid_restricted_hosts": {
                "declaration": "unknown",
                "evidence": "NOT TESTED"
            }
        }
    })
}

/// Persists the Phase 2 capability boundary into `adapter_registry`.
pub fn register_capabilities(journal: &Journal) {
    let caps = system_observer_capabilities().to_string();
    journal.adapter(AdapterRecord {
        adapter: "system-observer".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        capabilities: caps,
        verified_wall: Some(clock::wall_now_rfc3339_ms()),
        notes: Some("0-E §8.2 verbatim known-lossy capability boundary".into()),
    });
}
