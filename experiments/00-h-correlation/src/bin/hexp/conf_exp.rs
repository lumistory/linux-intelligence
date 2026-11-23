//! Experiment 4: the five credibility levels as a total, checkable function,
//! verified against 24 hand-built scenarios plus a monotonicity property.

use corr0h::confidence::{classify, Corroboration, Evidence, FileSide, Level, Lineage, Rule};
use corr0h::envelope::{EvidenceStatus, SourceClass};
use corr0h::identity::{ActorIdentity, ProcKey};

use crate::common::R;

struct Scenario {
    name: &'static str,
    evidence: Evidence,
    expect: Level,
    expect_rule: Rule,
    note: &'static str,
}

fn corr(pairs: &[(SourceClass, bool)]) -> Vec<Corroboration> {
    pairs
        .iter()
        .map(|(c, ind)| Corroboration {
            class: *c,
            status: if *c == SourceClass::Declared {
                EvidenceStatus::Declared
            } else {
                EvidenceStatus::Observed
            },
            independent: *ind,
        })
        .collect()
}

fn mk(
    actor: Option<ActorIdentity>,
    file: FileSide,
    lin: Lineage,
    tw: bool,
    c: Vec<Corroboration>,
    sess_only: bool,
) -> Evidence {
    Evidence {
        actor,
        file_side: Some(file),
        lineage: Some(lin),
        time_within: Some(tw),
        corroboration: c,
        session_only: Some(sess_only),
    }
}

fn scenarios() -> Vec<Scenario> {
    use SourceClass::{Declared, KernelFs, KernelProc};
    let keyed = Some(ActorIdentity::Keyed(ProcKey::new(100, 500)));
    let amb = Some(ActorIdentity::Ambiguous(ProcKey::new(100, 500)));
    vec![
        Scenario { name: "actor itself is the reported writer, path tied, inside interval",
            evidence: mk(keyed, FileSide::PathTiedToCommand, Lineage::SelfObserved, true, vec![], false),
            expect: Level::Confirmed, expect_rule: Rule::R3DirectForkObserved,
            note: "same-source direct evidence" },
        Scenario { name: "one-hop fork chain, path tied, inside interval",
            evidence: mk(keyed, FileSide::PathTiedToCommand, Lineage::ForkChain { depth: 1 }, true, vec![], false),
            expect: Level::Confirmed, expect_rule: Rule::R3DirectForkObserved,
            note: "kernel-reported parent edge" },
        Scenario { name: "four-hop fork chain (pipeline), path tied",
            evidence: mk(keyed, FileSide::PathTiedToCommand, Lineage::ForkChain { depth: 4 }, true, vec![], false),
            expect: Level::Confirmed, expect_rule: Rule::R3DirectForkObserved,
            note: "depth does not weaken a real edge" },
        Scenario { name: "(pid,starttime) collision, everything else perfect",
            evidence: mk(amb, FileSide::PathTiedToCommand, Lineage::SelfObserved, true, vec![], false),
            expect: Level::Unknown, expect_rule: Rule::R0PidKeyCollision,
            note: "collision detected -> refuse" },
        Scenario { name: "collision plus two independent corroborations",
            evidence: mk(amb, FileSide::PathTiedToCommand, Lineage::SelfObserved, true,
                corr(&[(Declared, true), (KernelFs, true)]), false),
            expect: Level::Unknown, expect_rule: Rule::R0PidKeyCollision,
            note: "more evidence cannot outvote a broken identity" },
        Scenario { name: "only a shared session_id",
            evidence: mk(None, FileSide::None, Lineage::None, false, vec![], true),
            expect: Level::Unknown, expect_rule: Rule::R1SessionOnly,
            note: "session is not attribution" },
        Scenario { name: "no actor at all, file event present",
            evidence: mk(None, FileSide::PathObservedUntied, Lineage::None, true, vec![], false),
            expect: Level::Unknown, expect_rule: Rule::R1SessionOnly,
            note: "0-E: inotify carries no pid" },
        Scenario { name: "file event strictly outside the actor's interval",
            evidence: mk(keyed, FileSide::PathTiedToCommand, Lineage::ForkChain { depth: 1 }, false, vec![], false),
            expect: Level::Unknown, expect_rule: Rule::R2NoCommonWindow,
            note: "temporal impossibility" },
        Scenario { name: "declared hook + kernel fs agree, path tied, no fork chain",
            evidence: mk(keyed, FileSide::PathTiedToCommand, Lineage::None, true,
                corr(&[(Declared, true), (KernelFs, true)]), false),
            expect: Level::Strong, expect_rule: Rule::R4TwoIndependentSources,
            note: "two independent classes, one kernel" },
        Scenario { name: "declared hook + kernel proc agree, no file evidence",
            evidence: mk(keyed, FileSide::None, Lineage::None, true,
                corr(&[(Declared, true), (KernelProc, true)]), false),
            expect: Level::Strong, expect_rule: Rule::R4TwoIndependentSources,
            note: "kernel proc corroborates the command itself" },
        Scenario { name: "kernel proc + kernel fs agree",
            evidence: mk(keyed, FileSide::PathObservedUntied, Lineage::None, true,
                corr(&[(KernelProc, true), (KernelFs, true)]), false),
            expect: Level::Strong, expect_rule: Rule::R4TwoIndependentSources,
            note: "two kernel sources, still not Confirmed" },
        Scenario { name: "two declared shells report the same command (hook fired twice)",
            evidence: mk(keyed, FileSide::None, Lineage::None, true,
                corr(&[(Declared, true), (Declared, true)]), false),
            expect: Level::Weak, expect_rule: Rule::R6TimeOverlapOnly,
            note: "same class twice is one source, not two" },
        Scenario { name: "kernel fs claim marked derived from kernel proc",
            evidence: mk(keyed, FileSide::None, Lineage::None, true,
                corr(&[(KernelProc, true), (KernelFs, false)]), false),
            expect: Level::Weak, expect_rule: Rule::R6TimeOverlapOnly,
            note: "independence is per-source, not assumed from class alone" },
        Scenario { name: "fork chain inside interval, no filesystem evidence at all",
            evidence: mk(keyed, FileSide::None, Lineage::ForkChain { depth: 2 }, true, vec![], false),
            expect: Level::Probable, expect_rule: Rule::R5PidChainNoFileEvidence,
            note: "the canonical 'probably, but unproven' case" },
        Scenario { name: "fork chain inside interval, untied path observed",
            evidence: mk(keyed, FileSide::PathObservedUntied, Lineage::ForkChain { depth: 1 }, true, vec![], false),
            expect: Level::Probable, expect_rule: Rule::R5PidChainNoFileEvidence,
            note: "a path nobody tied to the command does not confirm it" },
        Scenario { name: "actor is the reported writer, but path untied",
            evidence: mk(keyed, FileSide::PathObservedUntied, Lineage::SelfObserved, true, vec![], false),
            expect: Level::Probable, expect_rule: Rule::R5PidChainNoFileEvidence,
            note: "writer identity strong, artefact link weak" },
        Scenario { name: "pid-only actor with a tied path inside the interval",
            evidence: mk(Some(ActorIdentity::PidOnly(100)), FileSide::PathTiedToCommand,
                Lineage::PidEqualityOnly, true, vec![], false),
            expect: Level::Weak, expect_rule: Rule::R6TimeOverlapOnly,
            note: "missing starttime removes the identity, keeps the interval" },
        Scenario { name: "pid-only actor, pid equality, no window overlap",
            evidence: mk(Some(ActorIdentity::PidOnly(100)), FileSide::None,
                Lineage::PidEqualityOnly, false, vec![], false),
            expect: Level::Unknown, expect_rule: Rule::R7Residual,
            note: "nothing holds" },
        Scenario { name: "keyed actor, no lineage, untied path, inside interval",
            evidence: mk(keyed, FileSide::PathObservedUntied, Lineage::None, true, vec![], false),
            expect: Level::Weak, expect_rule: Rule::R6TimeOverlapOnly,
            note: "time overlap only" },
        Scenario { name: "keyed actor, no file event, no lineage, inside interval",
            evidence: mk(keyed, FileSide::None, Lineage::None, true, vec![], false),
            expect: Level::Weak, expect_rule: Rule::R6TimeOverlapOnly,
            note: "still only time" },
        Scenario { name: "three independent classes (declared + proc + fs)",
            evidence: mk(keyed, FileSide::PathTiedToCommand, Lineage::None, true,
                corr(&[(Declared, true), (KernelProc, true), (KernelFs, true)]), false),
            expect: Level::Strong, expect_rule: Rule::R4TwoIndependentSources,
            note: "three sources still cap at Strong without a fork edge" },
        Scenario { name: "inferred-only corroboration, fork chain otherwise perfect",
            evidence: Evidence {
                actor: keyed,
                file_side: Some(FileSide::PathTiedToCommand),
                lineage: Some(Lineage::ForkChain { depth: 1 }),
                time_within: Some(true),
                corroboration: vec![Corroboration {
                    class: KernelProc,
                    status: EvidenceStatus::Inferred,
                    independent: true,
                }],
                session_only: Some(false),
            },
            expect: Level::Confirmed, expect_rule: Rule::R3DirectForkObserved,
            note: "inferred inputs are filtered from R4, not from R3" },
        Scenario { name: "keyed actor, tied path, no time containment recorded",
            evidence: mk(keyed, FileSide::PathTiedToCommand, Lineage::ForkChain { depth: 1 }, false, vec![], false),
            expect: Level::Unknown, expect_rule: Rule::R2NoCommonWindow,
            note: "unknown containment is treated as no containment" },
        Scenario { name: "one kernel source corroborating only itself",
            evidence: mk(keyed, FileSide::None, Lineage::None, true,
                corr(&[(KernelProc, true)]), true),
            expect: Level::Weak, expect_rule: Rule::R6TimeOverlapOnly,
            note: "a single source is never 'two independent sources'" },
    ]
}

pub fn run() -> R<()> {
    println!();
    println!("## 4. Five-level credibility — 24 scenarios");
    println!();
    println!("Rule table, evaluated in this order, first match wins:");
    println!();
    println!("| rule | level | predicate (all conjuncts required) |");
    println!("|---|---|---|");
    println!("| R0 | Unknown | actor identity `Ambiguous` — (pid,starttime) collided |");
    println!("| R1 | Unknown | no actor, or the only link is a shared session_id |");
    println!("| R2 | Unknown | a file event exists and lies outside the actor's interval |");
    println!("| R3 | Confirmed | keyed actor AND (self-observed OR fork edge) AND inside interval AND path tied to the command |");
    println!("| R4 | Strong | >= 2 independent source classes AND >= 1 kernel-observed |");
    println!("| R5 | Probable | keyed actor AND (self-observed OR fork edge) AND inside interval AND no tied file evidence |");
    println!("| R6 | Weak | inside the interval and nothing more |");
    println!("| R7 | Unknown | residual |");
    println!();
    println!("Independence is defined, not felt: classes differ AND neither source is marked");
    println!("derived AND the pair is in INDEPENDENT_PAIRS (declared/proc, declared/fs,");
    println!("proc/fs). Two hooks firing twice are ONE source.");
    println!();
    println!("| # | scenario | expected | rule | actual | rule | verdict |");
    println!("|---|---|---|---|---|---|---|");
    let mut fails = 0;
    let list = scenarios();
    for (i, s) in list.iter().enumerate() {
        let v = classify(&s.evidence);
        let ok = v.level == s.expect && v.rule == s.expect_rule;
        if !ok {
            fails += 1;
        }
        println!("| S{:02} | {} | {} | {} | {} | {} | {} |",
            i + 1, s.name, s.expect.as_str(), s.expect_rule.as_str(),
            v.level.as_str(), v.rule.as_str(), if ok { "PASS" } else { "FAIL" });
    }
    println!();
    println!("{}/{} scenarios match the specified level AND rule.", list.len() - fails, list.len());

    println!();
    println!("### 4.0 Why each scenario sits where it does");
    println!();
    println!("| # | scenario | note |");
    println!("|---|---|---|");
    for (i, s) in list.iter().enumerate() {
        println!("| S{:02} | {} | {} |", i + 1, s.name, s.note);
    }

    println!();
    println!("### 4.1 Level distribution over the 24 scenarios");
    println!();
    let mut counts = [0usize; 5];
    for s in &list {
        let v = classify(&s.evidence);
        let i = match v.level {
            Level::Confirmed => 0,
            Level::Strong => 1,
            Level::Probable => 2,
            Level::Weak => 3,
            Level::Unknown => 4,
        };
        counts[i] += 1;
    }
    println!("| level | count |");
    println!("|---|---|");
    for (lvl, n) in [
        (Level::Confirmed, counts[0]),
        (Level::Strong, counts[1]),
        (Level::Probable, counts[2]),
        (Level::Weak, counts[3]),
        (Level::Unknown, counts[4]),
    ] {
        println!("| {} | {n} |", lvl.as_str());
    }

    println!();
    println!("### 4.2 Monotonicity: adding a corroboration never lowers the level");
    println!();
    let mut violations = 0;
    for s in &list {
        let base = classify(&s.evidence);
        let mut more = s.evidence.clone();
        if !matches!(more.actor, Some(ActorIdentity::Ambiguous(_))) {
            more.corroboration.push(Corroboration {
                class: SourceClass::KernelFs,
                status: EvidenceStatus::Observed,
                independent: true,
            });
        }
        let up = classify(&more);
        if up.level < base.level {
            violations += 1;
            println!("VIOLATION {} -> {}", base.level.as_str(), up.level.as_str());
        }
    }
    println!("violations: {violations} (must be 0)");
    if fails > 0 || violations > 0 {
        return Err(format!("{fails} scenario failures, {violations} monotonicity violations").into());
    }
    Ok(())
}
