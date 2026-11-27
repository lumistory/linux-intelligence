//! Correlation & Operation Context engine (P4-01 .. P4-10).
//!
//! Reads recent events from `Journal`, runs two-level dedup + R0–R7 classification,
//! creates operations (`operations`), attaches events many-to-many with per-edge
//! credibility (`event_operations`), and writes non-causal relations
//! (`event_relations` + `evidence`).

pub mod confidence;
pub mod dedup_engine {
    pub use super::engine::*;
}
pub mod relations;
pub mod reorder;

pub mod engine {
    use std::collections::HashMap;

    use li_journal::Journal;
    use li_journal::read::EventRecord;
    use li_journal::write::{Evidence as EvidenceRow, Operation, Write};
    use li_proto::CredibilityLevel;
    use li_proto::event::{EvidenceStatus, SourceClass};

    use super::confidence::{
        ActorIdentity, Corroboration, Evidence, FileSide, Lineage, ProcKey, classify,
    };
    use super::relations::RelationKind;
    use super::reorder::{DedupDecision, FingerprintPolicy, TwoLevelDedup};

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct CorrelationStats {
        pub events_scanned: usize,
        pub operations_created: usize,
        pub edges_attached: usize,
        pub relations_written: usize,
        pub corroborations_linked: usize,
    }

    fn source_class_of(src: &str) -> SourceClass {
        match src {
            "proc" => SourceClass::KernelProc,
            "fs" => SourceClass::KernelFs,
            _ => SourceClass::Declared,
        }
    }

    fn evidence_status_of(s: &str) -> EvidenceStatus {
        match s {
            "observed" => EvidenceStatus::Observed,
            "inferred" => EvidenceStatus::Inferred,
            _ => EvidenceStatus::Declared,
        }
    }

    fn actor_from_event(
        ev: &EventRecord,
        ambiguous_keys: &HashMap<(i32, u64), bool>,
    ) -> ActorIdentity {
        let pid = ev
            .payload
            .get("pid")
            .and_then(|v| v.as_i64())
            .and_then(|p| i32::try_from(p).ok());
        let st = ev.payload.get("starttime").and_then(|v| v.as_u64());
        match (pid, st) {
            (Some(p), Some(s)) => {
                let key = ProcKey::new(p, s);
                let is_amb = ev
                    .payload
                    .get("ambiguous")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    || ambiguous_keys.get(&(p, s)).copied().unwrap_or(false);
                if is_amb {
                    ActorIdentity::Ambiguous(key)
                } else {
                    ActorIdentity::Keyed(key)
                }
            }
            _ => ActorIdentity::Absent,
        }
    }

    /// Correlates up to `limit` recent events in `journal` into `operations`,
    /// `event_operations`, `event_relations`, and `evidence` rows.
    pub fn correlate_recent(journal: &Journal, limit: i64) -> Result<CorrelationStats, String> {
        let reader = journal.reader().map_err(|e| e.to_string())?;
        let mut events = reader
            .query_events(None, None, None, limit)
            .map_err(|e| e.to_string())?;
        // Process oldest -> newest.
        events.reverse();

        // Load sticky process ambiguity flags from `processes`.
        let mut ambiguous_keys: HashMap<(i32, u64), bool> = HashMap::new();
        {
            let mut st = reader
                .connection()
                .prepare(
                    "SELECT pid, starttime, max(ambiguous) FROM processes GROUP BY pid, starttime",
                )
                .map_err(|e| e.to_string())?;
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows.flatten() {
                if let (Ok(p), Ok(s)) = (i32::try_from(row.0), u64::try_from(row.1))
                    && row.2 > 0
                {
                    ambiguous_keys.insert((p, s), true);
                }
            }
        }

        let mut stats = CorrelationStats {
            events_scanned: events.len(),
            ..Default::default()
        };

        let mut dedup = TwoLevelDedup::new(FingerprintPolicy {
            window_ns: 500_000_000,
            require_same_session: false,
            require_same_process: false,
        });

        // Anchor operations on CommandFinished / ScriptFinished.
        let mut ops: Vec<(String, &EventRecord)> = Vec::new();
        for ev in &events {
            if matches!(ev.event_type.as_str(), "CommandFinished" | "ScriptFinished") {
                let op_id = format!("op-{}", ev.event_id);
                let label = ev
                    .payload
                    .get("command")
                    .or_else(|| ev.payload.get("program"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&ev.event_type)
                    .to_string();
                journal.submit(Write::Operation(Operation {
                    operation_id: op_id.clone(),
                    session_id: ev.session_id.clone(),
                    label,
                    intent: None,
                    state: "closed".into(),
                    started_mono_ns: ev.ts_mono_ns,
                }));
                let actor = actor_from_event(ev, &ambiguous_keys);
                let verdict = classify(&Evidence {
                    actor: Some(actor),
                    file_side: Some(FileSide::None),
                    lineage: Some(if matches!(actor, ActorIdentity::Keyed(_)) {
                        Lineage::SelfObserved
                    } else {
                        Lineage::None
                    }),
                    time_within: Some(true),
                    corroboration: vec![Corroboration {
                        class: source_class_of(&ev.source),
                        status: evidence_status_of(&ev.evidence_status),
                        independent: true,
                    }],
                    session_only: Some(false),
                    is_file_attribution: false,
                });
                journal.attach(&ev.event_id, &op_id, verdict.rule.as_str(), verdict.level);
                stats.operations_created += 1;
                stats.edges_attached += 1;
                ops.push((op_id, ev));
            }
        }

        // Corroborations + cross-source relations.
        for ev in &events {
            let (dec, canon) = dedup.offer(ev);
            if dec == DedupDecision::CorroborationOf
                && let Some(target_id) = canon
                && target_id != ev.event_id
            {
                let mut corrs = vec![Corroboration {
                    class: source_class_of(&ev.source),
                    status: evidence_status_of(&ev.evidence_status),
                    independent: true,
                }];
                if let Some(other) = events.iter().find(|x| x.event_id == target_id) {
                    corrs.push(Corroboration {
                        class: source_class_of(&other.source),
                        status: evidence_status_of(&other.evidence_status),
                        independent: true,
                    });
                }
                let verdict = classify(&Evidence {
                    actor: Some(actor_from_event(ev, &ambiguous_keys)),
                    file_side: Some(FileSide::None),
                    lineage: Some(Lineage::None),
                    time_within: Some(true),
                    corroboration: corrs,
                    session_only: Some(false),
                    is_file_attribution: false,
                });
                journal.relate(
                    &ev.event_id,
                    &target_id,
                    RelationKind::ObservedWithin.as_str(),
                    verdict.level,
                    verdict.rule.as_str(),
                );
                stats.corroborations_linked += 1;
                stats.relations_written += 1;
            }

            if matches!(
                ev.event_type.as_str(),
                "FileCreated" | "FileModified" | "FileDeleted" | "FileRenamed"
            ) {
                // Attach unattributed fs event to overlapping operations at Weak/Unknown (never Confirmed).
                for (op_id, op_ev) in &ops {
                    let delta = ev.ts_mono_ns.abs_diff(op_ev.ts_mono_ns);
                    if delta <= 2_000_000_000 {
                        let actor = actor_from_event(op_ev, &ambiguous_keys);
                        let cmd_text = op_ev
                            .payload
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let path_text = ev
                            .payload
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let tied = !path_text.is_empty() && cmd_text.contains(path_text);
                        let verdict = classify(&Evidence {
                            actor: Some(actor),
                            file_side: Some(if tied {
                                FileSide::PathTiedToCommand
                            } else {
                                FileSide::PathObservedUntied
                            }),
                            lineage: Some(Lineage::None),
                            time_within: Some(true),
                            corroboration: vec![
                                Corroboration {
                                    class: source_class_of(&op_ev.source),
                                    status: evidence_status_of(&op_ev.evidence_status),
                                    independent: true,
                                },
                                Corroboration {
                                    class: SourceClass::KernelFs,
                                    status: EvidenceStatus::Observed,
                                    independent: true,
                                },
                            ],
                            session_only: Some(false),
                            is_file_attribution: true,
                        });
                        // Cap file attribution at Probable (P4-09).
                        let capped = if verdict.level > CredibilityLevel::Probable {
                            CredibilityLevel::Probable
                        } else {
                            verdict.level
                        };
                        journal.attach(&ev.event_id, op_id, verdict.rule.as_str(), capped);
                        journal.relate(
                            &ev.event_id,
                            &op_ev.event_id,
                            RelationKind::ModifiedDuring.as_str(),
                            capped,
                            verdict.rule.as_str(),
                        );
                        stats.edges_attached += 1;
                        stats.relations_written += 1;
                    }
                }
            }
        }

        journal.flush().map_err(|e| e.to_string())?;

        // Attach corroboration `evidence` rows for the newly persisted relations.
        let rels = journal
            .reader()
            .map_err(|e| e.to_string())?
            .query_relations(None, 256)
            .map_err(|e| e.to_string())?;
        for r in rels.iter().take(stats.relations_written) {
            let (src, cls, status) = if r.kind == RelationKind::ModifiedDuring.as_str() {
                ("fs", "kernel_fs", "observed")
            } else {
                ("policy", "declared", "inferred")
            };
            journal.submit(Write::Evidence(EvidenceRow {
                relation_id: r.relation_id,
                source: src.into(),
                source_class: cls.into(),
                evidence_status: status.into(),
                independent: cls != "declared",
                confidence: None,
                detail: Some(format!("rule={}", r.rule)),
            }));
        }
        journal.flush().map_err(|e| e.to_string())?;
        Ok(stats)
    }
}
