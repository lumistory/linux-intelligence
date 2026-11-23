//! Operation Context: one event may belong to many operations, or to none.
//!
//! The tempting schema is `events.operation_id`. It is wrong for two measured
//! reasons:
//!  * a single `git commit` event belongs to the "edit code" operation *and*
//!    the "recover my work" operation that the user opened ten minutes later;
//!  * a background daemon's events belong to no operation at all, and forcing
//!    a synthetic "misc" operation destroys the ability to say "unattributed".
//!
//! So membership is a separate many-to-many relation with an explicit
//! `attached_by` (rule name), `level`, and a `detached_at` column, because an
//! event can stop belonging to an operation after evidence is retracted.

use crate::confidence::Level;

pub type EventId = String;
pub type OperationId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub event_id: EventId,
    pub operation_id: OperationId,
    /// Which classifier attached it; makes the edge auditable.
    pub attached_by: String,
    pub level: Level,
    /// Set when a retraction or a time-bounded operation ends the membership.
    pub detached_at_ns: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub id: OperationId,
    pub session_id: Option<String>,
    pub started_ns: u64,
    pub ended_ns: Option<u64>,
    pub label: String,
}

#[derive(Debug, Default)]
pub struct ContextStore {
    operations: Vec<Operation>,
    memberships: Vec<Membership>,
}

impl ContextStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_operation(&mut self, op: Operation) {
        self.operations.push(op);
    }

    pub fn attach(&mut self, m: Membership) {
        let exists = self
            .memberships
            .iter()
            .any(|x| x.event_id == m.event_id && x.operation_id == m.operation_id);
        if !exists {
            self.memberships.push(m);
        }
    }

    pub fn detach(&mut self, event: &str, operation: &str, at_ns: u64) {
        for m in self.memberships.iter_mut() {
            if m.event_id == event && m.operation_id == operation && m.detached_at_ns.is_none() {
                m.detached_at_ns = Some(at_ns);
            }
        }
    }

    /// Operations an event belongs to right now (0..n).
    pub fn operations_of(&self, event: &str) -> Vec<&Operation> {
        let live: Vec<&str> = self
            .memberships
            .iter()
            .filter(|m| m.event_id == event && m.detached_at_ns.is_none())
            .map(|m| m.operation_id.as_str())
            .collect();
        self.operations
            .iter()
            .filter(|o| live.contains(&o.id.as_str()))
            .collect()
    }

    /// Events in an operation, ordered by time; detached memberships excluded.
    pub fn events_of(&self, operation: &str) -> Vec<&str> {
        self.memberships
            .iter()
            .filter(|m| m.operation_id == operation && m.detached_at_ns.is_none())
            .map(|m| m.event_id.as_str())
            .collect()
    }

    /// Total membership rows, including detached ones: retraction must not
    /// erase history.
    pub fn memberships_count(&self) -> usize {
        self.memberships.len()
    }

    /// The explicit "belongs to nothing" answer, distinct from "not ingested".
    pub fn orphan_events<'a>(&self, all_events: &'a [EventId]) -> Vec<&'a EventId> {
        all_events
            .iter()
            .filter(|e| self.operations_of(e).is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(id: &str, label: &str) -> Operation {
        Operation {
            id: id.to_string(),
            session_id: Some("s1".into()),
            started_ns: 0,
            ended_ns: None,
            label: label.to_string(),
        }
    }

    fn mem(e: &str, o: &str) -> Membership {
        Membership {
            event_id: e.to_string(),
            operation_id: o.to_string(),
            attached_by: "R6".into(),
            level: Level::Probable,
            detached_at_ns: None,
        }
    }

    #[test]
    fn one_event_two_operations() {
        let mut s = ContextStore::new();
        s.add_operation(op("op1", "edit"));
        s.add_operation(op("op2", "recover"));
        s.attach(mem("e1", "op1"));
        s.attach(mem("e1", "op2"));
        assert_eq!(s.operations_of("e1").len(), 2);
    }

    #[test]
    fn event_with_no_operation_is_reported_not_dropped() {
        let mut s = ContextStore::new();
        s.add_operation(op("op1", "edit"));
        s.attach(mem("e1", "op1"));
        let all = vec!["e1".to_string(), "e9".to_string()];
        let orphans = s.orphan_events(&all);
        assert_eq!(orphans, vec![&"e9".to_string()]);
    }

    #[test]
    fn detach_removes_membership_without_deleting_history() {
        let mut s = ContextStore::new();
        s.add_operation(op("op1", "edit"));
        s.attach(mem("e1", "op1"));
        assert_eq!(s.operations_of("e1").len(), 1);
        s.detach("e1", "op1", 500);
        assert!(s.operations_of("e1").is_empty());
        assert_eq!(s.memberships.len(), 1);
    }
}
