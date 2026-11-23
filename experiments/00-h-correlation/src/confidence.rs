//! Five-level credibility classification.
//!
//! The whole module is one total function `classify(&Evidence) -> Verdict`.
//! There is no fallback heuristic outside the table: if a predicate does not
//! hold, the level drops. Rules are evaluated in a fixed precedence order and
//! the first rule whose predicate holds wins, so two rules can never both be
//! "the reason" and the reason string is always checkable.

use crate::envelope::{EvidenceStatus, SourceClass};
use crate::identity::ActorIdentity;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Unknown,
    Weak,
    Probable,
    Strong,
    Confirmed,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Confirmed => "Confirmed",
            Level::Strong => "Strong",
            Level::Probable => "Probable",
            Level::Weak => "Weak",
            Level::Unknown => "Unknown",
        }
    }
}

/// Which table row fired. Exhaustive: every classification names a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    R0PidKeyCollision,
    R1SessionOnly,
    R2NoCommonWindow,
    R3DirectForkObserved,
    R4TwoIndependentSources,
    R5PidChainNoFileEvidence,
    R6TimeOverlapOnly,
    R7Residual,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::R0PidKeyCollision => "R0 (pid,starttime) collision",
            Rule::R1SessionOnly => "R1 session identity only",
            Rule::R2NoCommonWindow => "R2 no overlapping time window",
            Rule::R3DirectForkObserved => "R3 direct fork evidence, inside interval",
            Rule::R4TwoIndependentSources => "R4 two independent sources, same fact",
            Rule::R5PidChainNoFileEvidence => "R5 pid chain inside window, no file-side evidence",
            Rule::R6TimeOverlapOnly => "R6 time-window overlap only",
            Rule::R7Residual => "R7 residual (nothing to stand on)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSide {
    /// No filesystem event at all for this candidate.
    None,
    /// inotify saw a path, but nothing ties the path to the command's text.
    PathObservedUntied,
    /// inotify saw a path that the command's argv also names.
    PathTiedToCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lineage {
    /// No process-side relationship between actor and the file writer.
    None,
    /// The actor *is* the process the kernel reported for the file event.
    SelfObserved,
    /// `(pid, starttime)` parent -> child edge walked in procfs, `depth` hops.
    ForkChain { depth: u8 },
    /// Bare `pid` equality only: the key collided or `starttime` was missing.
    PidEqualityOnly,
}

/// One source asserting something about the candidate relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Corroboration {
    pub class: SourceClass,
    pub status: EvidenceStatus,
    /// False if this source *derived* its claim from the other one (e.g. the
    /// shell hook reported what it was told by the script). Derived claims are
    /// not independent no matter how many of them there are.
    pub independent: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub actor: Option<ActorIdentity>,
    pub file_side: Option<FileSide>,
    pub lineage: Option<Lineage>,
    /// Raw timestamp of the file event lies inside the actor's live interval.
    pub time_within: Option<bool>,
    pub corroboration: Vec<Corroboration>,
    /// The only thing linking the two events is a shared `session_id`.
    pub session_only: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub level: Level,
    pub rule: Rule,
}

fn verdict(rule: Rule, level: Level) -> Verdict {
    Verdict { level, rule }
}

/// Distinct, genuinely independent, *kernel* confirmation classes.
///
/// "Independent" is not a vibe: two sources are independent iff
///   1. their `SourceClass` differs, and
///   2. neither marked itself as derived from the other, and
///   3. the pair is one of the combinations in `INDEPENDENT_PAIRS`.
pub const INDEPENDENT_PAIRS: &[(SourceClass, SourceClass)] = &[
    // declared command lifecycle vs kernel-observed process fact
    (SourceClass::Declared, SourceClass::KernelProc),
    // declared command lifecycle vs kernel-observed filesystem fact
    (SourceClass::Declared, SourceClass::KernelFs),
    // kernel process fact vs kernel filesystem fact
    (SourceClass::KernelProc, SourceClass::KernelFs),
];

pub fn independent_pair(a: SourceClass, b: SourceClass) -> bool {
    if a == b {
        return false;
    }
    INDEPENDENT_PAIRS
        .iter()
        .any(|(x, y)| (*x == a && *y == b) || (*x == b && *y == a))
}

fn independent_classes(ev: &Evidence) -> Vec<SourceClass> {
    let usable: Vec<&Corroboration> = ev
        .corroboration
        .iter()
        .filter(|c| c.independent && c.status != EvidenceStatus::Inferred)
        .collect();
    let mut out: Vec<SourceClass> = Vec::new();
    for c in &usable {
        if out.iter().any(|x| independent_pair(*x, c.class)) {
            if !out.contains(&c.class) {
                out.push(c.class);
            }
        } else if out.is_empty() {
            out.push(c.class);
        }
    }
    out
}

pub fn classify(ev: &Evidence) -> Verdict {
    let actor = ev.actor.unwrap_or(ActorIdentity::Absent);
    let file_side = ev.file_side.unwrap_or(FileSide::None);
    let lineage = ev.lineage.unwrap_or(Lineage::None);
    let time_within = ev.time_within.unwrap_or(false);
    let session_only = ev.session_only.unwrap_or(false);
    let classes = independent_classes(ev);
    let has_kernel = classes
        .iter()
        .any(|c| matches!(c, SourceClass::KernelProc | SourceClass::KernelFs));

    // R0: the identity key collided. Nothing about this actor can be trusted,
    // including a perfect-looking time overlap.
    if matches!(actor, ActorIdentity::Ambiguous(_)) {
        return verdict(Rule::R0PidKeyCollision, Level::Unknown);
    }

    // R1: there is no process identity at all, so the only thing left is the
    // session. A *keyed* actor is never demoted by this rule: asserting
    // `session_only` alongside a real key is a contradiction in the caller, and
    // the key wins because it is the stronger fact.
    let no_key = !matches!(actor, ActorIdentity::Keyed(_));
    if no_key && session_only && lineage == Lineage::None && file_side == FileSide::None {
        return verdict(Rule::R1SessionOnly, Level::Unknown);
    }
    if matches!(actor, ActorIdentity::Absent) && lineage == Lineage::None {
        return verdict(Rule::R1SessionOnly, Level::Unknown);
    }

    // R2: no interval intersection at all -> cannot be the same episode.
    if !time_within && file_side != FileSide::None {
        return verdict(Rule::R2NoCommonWindow, Level::Unknown);
    }

    // R3: Confirmed. Same-source direct evidence: keyed actor, a real fork
    // edge (or the actor itself is the reported writer), inside the interval,
    // and the path is tied to the command text.
    if matches!(actor, ActorIdentity::Keyed(_))
        && matches!(lineage, Lineage::SelfObserved | Lineage::ForkChain { .. })
        && time_within
        && file_side == FileSide::PathTiedToCommand
    {
        return verdict(Rule::R3DirectForkObserved, Level::Confirmed);
    }

    // R4: Strong. Two independent classes agree, at least one of them kernel.
    if classes.len() >= 2 && has_kernel {
        return verdict(Rule::R4TwoIndependentSources, Level::Strong);
    }

    // R5: Probable. Process-side chain plus a contained interval, but the
    // filesystem never *tied* an artefact to this command. An untied path is
    // explicitly not good enough: it is the 0-E case of "a file changed, nobody
    // knows who did it".
    if matches!(actor, ActorIdentity::Keyed(_))
        && matches!(lineage, Lineage::SelfObserved | Lineage::ForkChain { .. })
        && time_within
        && file_side != FileSide::PathTiedToCommand
    {
        return verdict(Rule::R5PidChainNoFileEvidence, Level::Probable);
    }

    // R6: Weak. Only the clocks overlap.
    if time_within {
        return verdict(Rule::R6TimeOverlapOnly, Level::Weak);
    }

    verdict(Rule::R7Residual, Level::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::ProcKey;

    fn keyed() -> ActorIdentity {
        ActorIdentity::Keyed(ProcKey::new(100, 500))
    }

    fn base() -> Evidence {
        Evidence {
            actor: Some(keyed()),
            file_side: Some(FileSide::PathTiedToCommand),
            lineage: Some(Lineage::ForkChain { depth: 1 }),
            time_within: Some(true),
            corroboration: vec![],
            session_only: Some(false),
        }
    }

    #[test]
    fn happy_path_is_confirmed() {
        let v = classify(&base());
        assert_eq!(v.level, Level::Confirmed);
        assert_eq!(v.rule, Rule::R3DirectForkObserved);
    }

    #[test]
    fn collision_beats_everything() {
        let mut e = base();
        e.actor = Some(ActorIdentity::Ambiguous(ProcKey::new(100, 500)));
        e.corroboration = vec![
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
        ];
        assert_eq!(classify(&e).level, Level::Unknown);
    }

    #[test]
    fn two_declared_sources_are_not_independent() {
        let mut e = base();
        e.file_side = Some(FileSide::None);
        e.corroboration = vec![
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
        ];
        assert_eq!(classify(&e).level, Level::Probable);
    }

    #[test]
    fn derived_claim_does_not_corroborate() {
        let mut e = base();
        e.file_side = Some(FileSide::None);
        e.corroboration = vec![
            Corroboration {
                class: SourceClass::KernelProc,
                status: EvidenceStatus::Observed,
                independent: true,
            },
            Corroboration {
                class: SourceClass::KernelFs,
                status: EvidenceStatus::Observed,
                independent: false, // derived from the proc event
            },
        ];
        assert_eq!(classify(&e).level, Level::Probable);
    }

    #[test]
    fn independence_matrix() {
        assert!(independent_pair(SourceClass::Declared, SourceClass::KernelFs));
        assert!(!independent_pair(SourceClass::Declared, SourceClass::Declared));
        assert!(!independent_pair(SourceClass::KernelProc, SourceClass::KernelProc));
    }
}
