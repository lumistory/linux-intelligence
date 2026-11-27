//! Relation vocabulary, causal-verb gate, and rendering cap (P4-05 / P4-10).

use li_proto::CredibilityLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    ObservedWithin,
    SpawnedBy,
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

    pub fn is_directional(self) -> bool {
        matches!(self, RelationKind::SpawnedBy)
    }

    /// Rejects `caused_by`, `causes`, `led_to`, `result_of`, and any unknown string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "observed_within" => Some(RelationKind::ObservedWithin),
            "spawned_by" => Some(RelationKind::SpawnedBy),
            "modified_during" => Some(RelationKind::ModifiedDuring),
            _ => None,
        }
    }

    /// Renders human-readable edge text capped by `kind` (P4-10): only
    /// `SpawnedBy` renders `"was spawned by"`; temporal kinds never emit causal verbs.
    pub fn render(self, subject: &str, object: &str, level: CredibilityLevel) -> String {
        match self {
            RelationKind::ObservedWithin => {
                format!("{subject} was observed within {object}'s window ({level})")
            }
            RelationKind::SpawnedBy => {
                format!("{subject} was spawned by {object} ({level})")
            }
            RelationKind::ModifiedDuring => {
                format!("{object} was modified during {subject}'s lifetime ({level})")
            }
        }
    }
}
