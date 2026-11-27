//! Five-level credibility classification (R0–R7, P4-06 / P4-07 / P4-09).
//!
//! Pure total function `classify(&Evidence) -> Verdict`, directly implementing
//! 0-H §4.4 and enforcing the P6 / P4-09 file-attribution ceiling (`Probable`).

use li_proto::CredibilityLevel;
use li_proto::event::{EvidenceStatus, SourceClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcKey {
    pub pid: i32,
    pub starttime: u64,
}

impl ProcKey {
    pub fn new(pid: i32, starttime: u64) -> Self {
        Self { pid, starttime }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorIdentity {
    Absent,
    Keyed(ProcKey),
    Ambiguous(ProcKey),
}

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
            Rule::R0PidKeyCollision => "R0",
            Rule::R1SessionOnly => "R1",
            Rule::R2NoCommonWindow => "R2",
            Rule::R3DirectForkObserved => "R3",
            Rule::R4TwoIndependentSources => "R4",
            Rule::R5PidChainNoFileEvidence => "R5",
            Rule::R6TimeOverlapOnly => "R6",
            Rule::R7Residual => "R7",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSide {
    None,
    PathObservedUntied,
    PathTiedToCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lineage {
    None,
    SelfObserved,
    ForkChain { depth: u8 },
    PidEqualityOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Corroboration {
    pub class: SourceClass,
    pub status: EvidenceStatus,
    pub independent: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub actor: Option<ActorIdentity>,
    pub file_side: Option<FileSide>,
    pub lineage: Option<Lineage>,
    pub time_within: Option<bool>,
    pub corroboration: Vec<Corroboration>,
    pub session_only: Option<bool>,
    /// True when classifying a filesystem modification edge (subject to P6 cap).
    pub is_file_attribution: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub level: CredibilityLevel,
    pub rule: Rule,
    pub note: Option<&'static str>,
}

const INDEPENDENT_PAIRS: &[(SourceClass, SourceClass)] = &[
    (SourceClass::Declared, SourceClass::KernelProc),
    (SourceClass::Declared, SourceClass::KernelFs),
    (SourceClass::KernelProc, SourceClass::KernelFs),
];

pub fn independent_pair(a: SourceClass, b: SourceClass) -> bool {
    a != b
        && INDEPENDENT_PAIRS
            .iter()
            .any(|(x, y)| (*x == a && *y == b) || (*x == b && *y == a))
}

fn independent_classes(ev: &Evidence) -> Vec<SourceClass> {
    let mut out = Vec::new();
    for c in ev
        .corroboration
        .iter()
        .filter(|c| c.independent && c.status != EvidenceStatus::Inferred)
    {
        if out.is_empty()
            || (out.iter().any(|x| independent_pair(*x, c.class)) && !out.contains(&c.class))
        {
            out.push(c.class);
        }
    }
    out
}

/// Evaluates R0–R7 in strict precedence order. First matching rule wins.
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

    // R0: identity key collided -> Unknown overrides everything.
    if matches!(actor, ActorIdentity::Ambiguous(_)) {
        return Verdict {
            level: CredibilityLevel::Unknown,
            rule: Rule::R0PidKeyCollision,
            note: None,
        };
    }

    // R1: session identity only, no actor key.
    let no_key = !matches!(actor, ActorIdentity::Keyed(_));
    if (no_key && session_only && lineage == Lineage::None && file_side == FileSide::None)
        || (matches!(actor, ActorIdentity::Absent) && lineage == Lineage::None)
    {
        return Verdict {
            level: CredibilityLevel::Unknown,
            rule: Rule::R1SessionOnly,
            note: None,
        };
    }

    // R2: no overlapping time window.
    if !time_within && file_side != FileSide::None {
        return Verdict {
            level: CredibilityLevel::Unknown,
            rule: Rule::R2NoCommonWindow,
            note: None,
        };
    }

    // R3: Direct fork/self observed + tied path inside window.
    if matches!(actor, ActorIdentity::Keyed(_))
        && matches!(lineage, Lineage::SelfObserved | Lineage::ForkChain { .. })
        && time_within
        && file_side == FileSide::PathTiedToCommand
    {
        // P4-09 / P6: unprivileged file attribution cannot reach Confirmed;
        // an attempted Confirmed file claim must downgrade to Unknown + `path_not_tied`.
        if ev.is_file_attribution {
            return Verdict {
                level: CredibilityLevel::Unknown,
                rule: Rule::R3DirectForkObserved,
                note: Some("path_not_tied"),
            };
        }
        return Verdict {
            level: CredibilityLevel::Confirmed,
            rule: Rule::R3DirectForkObserved,
            note: None,
        };
    }

    // R4: Two independent source classes with >= 1 kernel-observed.
    if classes.len() >= 2 && has_kernel {
        return Verdict {
            level: CredibilityLevel::Strong,
            rule: Rule::R4TwoIndependentSources,
            note: None,
        };
    }

    // R5: Probable (keyed actor + fork/self chain inside window, untied path).
    if matches!(actor, ActorIdentity::Keyed(_))
        && matches!(lineage, Lineage::SelfObserved | Lineage::ForkChain { .. })
        && time_within
        && file_side != FileSide::PathTiedToCommand
    {
        return Verdict {
            level: CredibilityLevel::Probable,
            rule: Rule::R5PidChainNoFileEvidence,
            note: None,
        };
    }

    // R6: Weak (time-window overlap only).
    if time_within {
        return Verdict {
            level: CredibilityLevel::Weak,
            rule: Rule::R6TimeOverlapOnly,
            note: None,
        };
    }

    Verdict {
        level: CredibilityLevel::Unknown,
        rule: Rule::R7Residual,
        note: None,
    }
}
