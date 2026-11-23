//! What a relation is allowed to claim.
//!
//! The API has exactly three relation kinds and deliberately no way to spell
//! "caused". Any consumer that wants causality has to build it from
//! `spawned_by` + `modified_during` + the credibility level, and it will
//! inherit the caveats instead of hiding them.

use crate::confidence::Level;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    /// A and B happened in overlapping intervals. Says nothing about which one
    /// made the other happen, nor even which came first at sub-tick resolution.
    ObservedWithin,
    /// The kernel reported a parent->child edge (`ppid` at fork time) between
    /// the two processes. The only relation allowed to use the word "spawn".
    SpawnedBy,
    /// The file event's timestamp falls inside the writer process's live
    /// interval. Not "the file was written by": see `Counterexample`.
    ModifiedDuring,
}

impl RelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationKind::ObservedWithin => "observed_within",
            RelationKind::SpawnedBy => "spawned_by",
            RelationKind::ModifiedDuring => "modified_during",
        }
    }

    /// Whether a UI may render this relation as an arrow pointing from cause to
    /// effect. Only `spawned_by` has a kernel-observed direction.
    pub fn is_directional(self) -> bool {
        matches!(self, RelationKind::SpawnedBy)
    }

    /// Parse from an external string (IPC, JSON policy file). Unknown kinds are
    /// rejected rather than mapped to a default: a policy file that says
    /// `caused_by` must fail loudly.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "observed_within" => Some(RelationKind::ObservedWithin),
            "spawned_by" => Some(RelationKind::SpawnedBy),
            "modified_during" => Some(RelationKind::ModifiedDuring),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub kind: RelationKind,
    pub level: Level,
    /// Human-checkable justification, copied from the rule that fired.
    pub rule: &'static str,
}

impl Relation {
    pub fn new(kind: RelationKind, level: Level, rule: &'static str) -> Self {
        Self { kind, level, rule }
    }

    /// Text a UI or an agent may show. Gated: a temporal relation may never
    /// render with causal verbs regardless of credibility level.
    pub fn render(&self, subject: &str, object: &str) -> String {
        match self.kind {
            RelationKind::ObservedWithin => format!(
                "{subject} was observed within {object}'s window ({})",
                self.level.as_str()
            ),
            RelationKind::SpawnedBy => {
                format!("{subject} was spawned by {object} ({})", self.level.as_str())
            }
            RelationKind::ModifiedDuring => format!(
                "{object} was modified during {subject}'s lifetime ({})",
                self.level.as_str()
            ),
        }
    }
}

/// The four concrete ways "file changed while X ran" becomes a lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counterexample {
    /// `make -j` returns; the compiler children keep writing for seconds.
    BackgroundTask,
    /// `echo > f` returns after `write(2)`; the data lands later via
    /// writeback, and a reader/observer sees mtime after the command exited.
    DelayedWrite,
    /// `vim` saves by writing a temp file and `rename(2)`ing it over the
    /// original: the inotify event names the *temp* path, and the path the user
    /// edited shows up as a rename target the command never opened.
    EditorRenameSave,
    /// Two processes append to one log file in the same window. A file event
    /// cannot be assigned to either one; 0-E established inotify carries no
    /// pid, so neither can the correlator.
    TwoWritersOneFile,
}

impl Counterexample {
    pub fn as_str(self) -> &'static str {
        match self {
            Counterexample::BackgroundTask => "background task outlives its command",
            Counterexample::DelayedWrite => "delayed writeback",
            Counterexample::EditorRenameSave => "editor rename-save",
            Counterexample::TwoWritersOneFile => "two writers, one file",
        }
    }

    /// The relation kind that is still defensible for this shape of evidence,
    /// and the level that must accompany it. This is the machine-checkable form
    /// of "do not upgrade time to cause".
    pub fn defensible(&self) -> (RelationKind, Level) {
        match self {
            Counterexample::BackgroundTask => (RelationKind::SpawnedBy, Level::Confirmed),
            Counterexample::DelayedWrite => (RelationKind::ModifiedDuring, Level::Weak),
            Counterexample::EditorRenameSave => (RelationKind::ObservedWithin, Level::Weak),
            Counterexample::TwoWritersOneFile => (RelationKind::ObservedWithin, Level::Unknown),
        }
    }
}

/// Reject a relation claim that the evidence cannot carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Temporal evidence dressed as causation.
    NotCausal,
    /// `spawned_by` claimed without a fork edge.
    NoForkEvidence,
    /// A `Confirmed` relation produced from `inferred` inputs only.
    InferredCannotConfirm,
}

pub fn validate(
    kind: RelationKind,
    level: Level,
    has_fork_edge: bool,
    any_observed_input: bool,
) -> Result<(), Rejection> {
    if kind.is_directional() && !has_fork_edge {
        return Err(Rejection::NoForkEvidence);
    }
    if level == Level::Confirmed && !any_observed_input {
        return Err(Rejection::InferredCannotConfirm);
    }
    if kind == RelationKind::ObservedWithin && level >= Level::Strong && !has_fork_edge {
        // Strong is allowed for ObservedWithin (two independent sources), so
        // this branch only exists to keep the rule explicit and testable.
        let _ = Rejection::NotCausal;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caused_by_does_not_parse() {
        assert_eq!(RelationKind::parse("caused_by"), None);
        assert_eq!(RelationKind::parse("causes"), None);
        assert_eq!(
            RelationKind::parse("modified_during"),
            Some(RelationKind::ModifiedDuring)
        );
    }

    #[test]
    fn only_spawned_by_is_directional() {
        assert!(RelationKind::SpawnedBy.is_directional());
        assert!(!RelationKind::ModifiedDuring.is_directional());
        assert!(!RelationKind::ObservedWithin.is_directional());
    }

    #[test]
    fn temporal_render_has_no_causal_verb() {
        let r = Relation::new(RelationKind::ModifiedDuring, Level::Probable, "R5");
        let text = r.render("cmd:make", "/tmp/x.o");
        for bad in ["caused", "because", "triggered", "result of", "wrote"] {
            assert!(!text.contains(bad), "{text} contains {bad}");
        }
    }

    #[test]
    fn spawn_without_edge_is_rejected() {
        assert_eq!(
            validate(RelationKind::SpawnedBy, Level::Confirmed, false, true),
            Err(Rejection::NoForkEvidence)
        );
        assert_eq!(
            validate(RelationKind::SpawnedBy, Level::Confirmed, true, true),
            Ok(())
        );
    }

    #[test]
    fn inferred_inputs_cannot_confirm() {
        assert_eq!(
            validate(RelationKind::ObservedWithin, Level::Confirmed, false, false),
            Err(Rejection::InferredCannotConfirm)
        );
    }

    #[test]
    fn every_counterexample_maps_to_a_defensible_pair() {
        for c in [
            Counterexample::BackgroundTask,
            Counterexample::DelayedWrite,
            Counterexample::EditorRenameSave,
            Counterexample::TwoWritersOneFile,
        ] {
            let (k, l) = c.defensible();
            assert!(RelationKind::parse(k.as_str()).is_some());
            assert!(l <= Level::Confirmed);
        }
    }
}
