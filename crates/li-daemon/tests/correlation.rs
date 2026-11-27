use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use li_daemon::correlation::confidence::{
    ActorIdentity, Corroboration, Evidence, FileSide, Lineage, ProcKey, Rule, classify,
};
use li_daemon::correlation::engine::correlate_recent;
use li_daemon::correlation::relations::RelationKind;
use li_daemon::correlation::reorder::{
    DedupDecision, FingerprintPolicy, ReorderOutput, Reorderer, TwoLevelDedup,
};
use li_journal::read::EventRecord;
use li_journal::write::{Process, Session, Write};
use li_journal::{Config, Journal};
use li_proto::event::{EvidenceStatus, SourceClass};
use li_proto::{CredibilityLevel, Event, EventId, EventType, SessionId, Source};

fn tempdir(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("li-corr-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&p).unwrap();
    p
}

fn keyed() -> ActorIdentity {
    ActorIdentity::Keyed(ProcKey::new(100, 500))
}

#[test]
fn all_24_credibility_scenarios_and_monotonicity_hold_with_zero_violations() {
    let scenarios: Vec<(&str, Evidence, CredibilityLevel, Rule)> = vec![
        // 1-4: R0 PID key collision overrides everything
        (
            "s01_r0_ambiguous_overrides_r3",
            Evidence {
                actor: Some(ActorIdentity::Ambiguous(ProcKey::new(100, 500))),
                file_side: Some(FileSide::PathTiedToCommand),
                lineage: Some(Lineage::ForkChain { depth: 1 }),
                time_within: Some(true),
                corroboration: vec![],
                session_only: Some(false),
                is_file_attribution: false,
            },
            CredibilityLevel::Unknown,
            Rule::R0PidKeyCollision,
        ),
        (
            "s02_r0_ambiguous_overrides_r4",
            Evidence {
                actor: Some(ActorIdentity::Ambiguous(ProcKey::new(100, 500))),
                file_side: Some(FileSide::PathObservedUntied),
                lineage: Some(Lineage::SelfObserved),
                time_within: Some(true),
                corroboration: vec![
                    Corroboration {
                        class: SourceClass::Declared,
                        status: EvidenceStatus::Declared,
                        independent: true,
                    },
                    Corroboration {
                        class: SourceClass::KernelProc,
                        status: EvidenceStatus::Observed,
                        independent: true,
                    },
                ],
                session_only: Some(false),
                is_file_attribution: false,
            },
            CredibilityLevel::Unknown,
            Rule::R0PidKeyCollision,
        ),
        (
            "s03_r0_ambiguous_with_time",
            Evidence {
                actor: Some(ActorIdentity::Ambiguous(ProcKey::new(2, 10))),
                time_within: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R0PidKeyCollision,
        ),
        (
            "s04_r0_ambiguous_bare",
            Evidence {
                actor: Some(ActorIdentity::Ambiguous(ProcKey::new(3, 10))),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R0PidKeyCollision,
        ),
        // 5-7: R1 Session identity only / absent actor
        (
            "s05_r1_session_only",
            Evidence {
                actor: Some(ActorIdentity::Absent),
                session_only: Some(true),
                time_within: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R1SessionOnly,
        ),
        (
            "s06_r1_absent_no_lineage",
            Evidence {
                actor: Some(ActorIdentity::Absent),
                file_side: Some(FileSide::PathObservedUntied),
                lineage: Some(Lineage::None),
                time_within: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R1SessionOnly,
        ),
        (
            "s07_r1_pid_equality_session_only",
            Evidence {
                actor: None,
                session_only: Some(true),
                lineage: Some(Lineage::None),
                file_side: Some(FileSide::None),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R1SessionOnly,
        ),
        // 8-10: R2 No overlapping time window
        (
            "s08_r2_no_time_window_keyed",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathTiedToCommand),
                lineage: Some(Lineage::SelfObserved),
                time_within: Some(false),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R2NoCommonWindow,
        ),
        (
            "s09_r2_no_time_window_untied",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathObservedUntied),
                lineage: Some(Lineage::ForkChain { depth: 2 }),
                time_within: Some(false),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R2NoCommonWindow,
        ),
        // 10-13: R3 Confirmed (process/non-file) and file-attribution ceiling (P4-09)
        (
            "s10_r3_confirmed_self",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathTiedToCommand),
                lineage: Some(Lineage::SelfObserved),
                time_within: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Confirmed,
            Rule::R3DirectForkObserved,
        ),
        (
            "s11_r3_confirmed_fork_chain",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathTiedToCommand),
                lineage: Some(Lineage::ForkChain { depth: 1 }),
                time_within: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Confirmed,
            Rule::R3DirectForkObserved,
        ),
        (
            "s12_r3_confirmed_ignores_contradictory_session_only",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathTiedToCommand),
                lineage: Some(Lineage::ForkChain { depth: 2 }),
                time_within: Some(true),
                session_only: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Confirmed,
            Rule::R3DirectForkObserved,
        ),
        (
            "s13_r3_file_attribution_capped_to_unknown_path_not_tied",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathTiedToCommand),
                lineage: Some(Lineage::ForkChain { depth: 1 }),
                time_within: Some(true),
                is_file_attribution: true,
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R3DirectForkObserved,
        ),
        // 14-17: R4 Strong (two independent source classes, >=1 kernel)
        (
            "s14_r4_declared_plus_kernel_proc",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathObservedUntied),
                lineage: Some(Lineage::SelfObserved),
                time_within: Some(true),
                corroboration: vec![
                    Corroboration {
                        class: SourceClass::Declared,
                        status: EvidenceStatus::Declared,
                        independent: true,
                    },
                    Corroboration {
                        class: SourceClass::KernelProc,
                        status: EvidenceStatus::Observed,
                        independent: true,
                    },
                ],
                ..Default::default()
            },
            CredibilityLevel::Strong,
            Rule::R4TwoIndependentSources,
        ),
        (
            "s15_r4_declared_plus_kernel_fs",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathObservedUntied),
                lineage: Some(Lineage::None),
                time_within: Some(true),
                corroboration: vec![
                    Corroboration {
                        class: SourceClass::Declared,
                        status: EvidenceStatus::Declared,
                        independent: true,
                    },
                    Corroboration {
                        class: SourceClass::KernelFs,
                        status: EvidenceStatus::Observed,
                        independent: true,
                    },
                ],
                ..Default::default()
            },
            CredibilityLevel::Strong,
            Rule::R4TwoIndependentSources,
        ),
        (
            "s16_r4_kernel_proc_plus_kernel_fs",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathObservedUntied),
                lineage: Some(Lineage::None),
                time_within: Some(true),
                corroboration: vec![
                    Corroboration {
                        class: SourceClass::KernelProc,
                        status: EvidenceStatus::Observed,
                        independent: true,
                    },
                    Corroboration {
                        class: SourceClass::KernelFs,
                        status: EvidenceStatus::Observed,
                        independent: true,
                    },
                ],
                ..Default::default()
            },
            CredibilityLevel::Strong,
            Rule::R4TwoIndependentSources,
        ),
        (
            "s17_r4_with_inferred_still_uses_independent_observed",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::None),
                lineage: Some(Lineage::None),
                time_within: Some(true),
                corroboration: vec![
                    Corroboration {
                        class: SourceClass::Declared,
                        status: EvidenceStatus::Declared,
                        independent: true,
                    },
                    Corroboration {
                        class: SourceClass::KernelProc,
                        status: EvidenceStatus::Observed,
                        independent: true,
                    },
                    Corroboration {
                        class: SourceClass::KernelFs,
                        status: EvidenceStatus::Inferred,
                        independent: true,
                    },
                ],
                ..Default::default()
            },
            CredibilityLevel::Strong,
            Rule::R4TwoIndependentSources,
        ),
        // 18-20: R5 Probable
        (
            "s18_r5_self_no_file_evidence",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::None),
                lineage: Some(Lineage::SelfObserved),
                time_within: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Probable,
            Rule::R5PidChainNoFileEvidence,
        ),
        (
            "s19_r5_fork_chain_untied_path",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathObservedUntied),
                lineage: Some(Lineage::ForkChain { depth: 2 }),
                time_within: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Probable,
            Rule::R5PidChainNoFileEvidence,
        ),
        (
            "s20_r5_two_same_class_hooks_are_not_independent",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::None),
                lineage: Some(Lineage::SelfObserved),
                time_within: Some(true),
                corroboration: vec![
                    Corroboration {
                        class: SourceClass::Declared,
                        status: EvidenceStatus::Declared,
                        independent: true,
                    },
                    Corroboration {
                        class: SourceClass::Declared,
                        status: EvidenceStatus::Declared,
                        independent: true,
                    },
                ],
                ..Default::default()
            },
            CredibilityLevel::Probable,
            Rule::R5PidChainNoFileEvidence,
        ),
        // 21-22: R6 Weak
        (
            "s21_r6_time_overlap_only_no_lineage",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::PathObservedUntied),
                lineage: Some(Lineage::None),
                time_within: Some(true),
                ..Default::default()
            },
            CredibilityLevel::Weak,
            Rule::R6TimeOverlapOnly,
        ),
        (
            "s22_r6_derived_corroboration_not_independent",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::None),
                lineage: Some(Lineage::PidEqualityOnly),
                time_within: Some(true),
                corroboration: vec![
                    Corroboration {
                        class: SourceClass::Declared,
                        status: EvidenceStatus::Declared,
                        independent: true,
                    },
                    Corroboration {
                        class: SourceClass::KernelProc,
                        status: EvidenceStatus::Observed,
                        independent: false,
                    },
                ],
                ..Default::default()
            },
            CredibilityLevel::Weak,
            Rule::R6TimeOverlapOnly,
        ),
        // 23-24: R7 Residual
        (
            "s23_r7_keyed_outside_time",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::None),
                lineage: Some(Lineage::None),
                time_within: Some(false),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R7Residual,
        ),
        (
            "s24_r7_keyed_with_pid_equality_no_time",
            Evidence {
                actor: Some(keyed()),
                file_side: Some(FileSide::None),
                lineage: Some(Lineage::PidEqualityOnly),
                time_within: Some(false),
                ..Default::default()
            },
            CredibilityLevel::Unknown,
            Rule::R7Residual,
        ),
    ];

    assert_eq!(scenarios.len(), 24);
    for (name, ev, want_level, want_rule) in scenarios {
        let got = classify(&ev);
        assert_eq!(
            (got.level, got.rule),
            (want_level, want_rule),
            "scenario {name} failed"
        );
        if name == "s13_r3_file_attribution_capped_to_unknown_path_not_tied" {
            assert_eq!(got.note, Some("path_not_tied"));
        }

        // Monotonicity check: adding a corroboration must never lower credibility level
        // (unless R0 collision or file-attribution cap applies).
        if got.rule != Rule::R0PidKeyCollision && !ev.is_file_attribution {
            let mut extra = ev.clone();
            extra.corroboration.push(Corroboration {
                class: SourceClass::KernelProc,
                status: EvidenceStatus::Observed,
                independent: true,
            });
            let after = classify(&extra);
            assert!(
                after.level >= got.level,
                "monotonicity violation on {name}: {:?} -> {:?}",
                got.level,
                after.level
            );
        }
    }
}

#[test]
fn relations_reject_caused_and_render_without_causal_verbs() {
    for bad in ["caused_by", "causes", "led_to", "result_of"] {
        assert_eq!(RelationKind::parse(bad), None);
    }
    for kind in [
        RelationKind::ObservedWithin,
        RelationKind::SpawnedBy,
        RelationKind::ModifiedDuring,
    ] {
        let rendered = kind.render("cmd", "file", CredibilityLevel::Probable);
        assert!(!rendered.to_ascii_lowercase().contains("caused"));
        if kind == RelationKind::SpawnedBy {
            assert!(rendered.contains("was spawned by"));
        } else {
            assert!(!rendered.contains("was spawned by"));
        }
    }
}

#[test]
fn reorderer_preserves_late_arrivals_and_dedup_keeps_rows_as_corroboration() {
    let mk = |id: &str, ts: i64, seq: i64, sid: Option<&str>, pid: i64, st: u64| EventRecord {
        ingest_seq: seq,
        event_id: id.into(),
        source: "shell.bash".into(),
        session_id: sid.map(str::to_string),
        sequence: seq,
        ts_mono_ns: ts,
        ts_wall: "2026-11-27T06:00:00.000Z".into(),
        event_type: "CommandFinished".into(),
        type_class: "proc.end".into(),
        evidence_status: "declared".into(),
        payload: serde_json::json!({"command": "make", "pid": pid, "starttime": st}),
        redaction_rules: None,
    };

    // Reorderer: 1 ms grace
    let mut re = Reorderer::new(1_000_000);
    assert!(
        re.push(mk("a", 5_000_000, 1, Some("s1"), 10, 1), 5_500_000)
            .is_empty()
    );
    let flushed = re.advance(7_000_000);
    assert!(matches!(flushed.as_slice(), [ReorderOutput::InOrder(e)] if e.event_id == "a"));
    // Late event arriving behind watermark 6_000_000 becomes first-class LateArrival.
    let late = re.push(mk("b", 4_000_000, 2, Some("s1"), 10, 1), 7_000_000);
    assert!(matches!(late.as_slice(), [ReorderOutput::LateArrival(e)] if e.event_id == "b"));

    // Fingerprint distinguishes two concurrent processes with same command in different sessions/pids.
    let mut dd = TwoLevelDedup::new(FingerprintPolicy::default());
    assert_eq!(
        dd.offer(&mk("e1", 10_000_000, 1, Some("s1"), 10, 100)).0,
        DedupDecision::Fresh
    );
    assert_eq!(
        dd.offer(&mk("e2", 10_000_000, 2, Some("s2"), 11, 101)).0,
        DedupDecision::Fresh
    );
    assert_eq!(
        dd.offer(&mk("e3", 10_000_000, 3, Some("s1"), 10, 100)).0,
        DedupDecision::CorroborationOf
    );
}

#[test]
fn correlate_recent_populates_operations_edges_relations_and_caps_files_at_probable() {
    let root = tempdir("engine");
    let mut j = Journal::open(&root.join("journal.db"), Config::default()).unwrap();
    j.migrate().unwrap();

    let sid = SessionId::new("corr-sess").unwrap();
    j.session(Session {
        session_id: "corr-sess".into(),
        parent_session: None,
        adapter: "bash".into(),
        adapter_version: None,
        shell_pid: Some(200),
        started_wall: "2026-11-27T06:00:00.000Z".into(),
        started_mono_ns: 1_000_000_000,
        cwd: Some("/tmp".into()),
        host: None,
        boot_id: None,
    });
    j.submit(Write::Process(Process {
        pid: 200,
        starttime: 99,
        incarnation_seq: 0,
        comm: Some("bash".into()),
        ppid: Some(1),
        session_id: Some("corr-sess".into()),
        first_seen_mono_ns: 1_000_000_000,
        ambiguous: false,
    }));

    let cmd_ev = Event::new(
        EventId::new("cmd-1").unwrap(),
        Source::ShellBash,
        Some(sid),
        0,
        1_500_000_000,
        "2026-11-27T06:00:01.000Z",
        EventType::new("CommandFinished").unwrap(),
        serde_json::json!({"command": "touch /tmp/a.txt", "pid": 200, "starttime": 99})
            .as_object()
            .unwrap()
            .clone(),
        EvidenceStatus::Declared,
    )
    .unwrap();
    let fs_ev = Event::new(
        EventId::new("fs-1").unwrap(),
        Source::Fs,
        None,
        0,
        1_600_000_000,
        "2026-11-27T06:00:01.100Z",
        EventType::new("FileCreated").unwrap(),
        serde_json::json!({"path": "/tmp/a.txt"})
            .as_object()
            .unwrap()
            .clone(),
        EvidenceStatus::Observed,
    )
    .unwrap();
    j.event(&cmd_ev).unwrap();
    j.event(&fs_ev).unwrap();
    j.flush().unwrap();

    let stats = correlate_recent(&j, 100).unwrap();
    assert_eq!(stats.operations_created, 1);
    assert!(stats.edges_attached >= 2);
    assert!(stats.relations_written >= 1);

    let r = j.reader().unwrap();
    assert_eq!(r.count("operations").unwrap(), 1);
    assert!(r.count("event_operations").unwrap() >= 2);
    assert!(r.count("evidence").unwrap() >= 1);
    let rels = r.query_relations(None, 10).unwrap();
    assert!(!rels.is_empty());
    for rel in &rels {
        assert_ne!(rel.kind, "caused_by");
        assert!(
            matches!(
                rel.credibility_level.as_str(),
                "unknown" | "weak" | "probable"
            ),
            "file attribution must not exceed probable, got {}",
            rel.credibility_level
        );
    }

    let _ = fs::remove_dir_all(root);
}
