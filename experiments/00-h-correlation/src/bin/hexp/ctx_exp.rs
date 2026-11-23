//! Experiment 6: Operation Context as a 0..n relation.

use corr0h::confidence::Level;
use corr0h::context::{ContextStore, Membership, Operation};

use crate::common::{R, TS0};

fn attach(e: &str, o: &str, by: &str, lvl: Level) -> Membership {
    Membership {
        event_id: e.into(),
        operation_id: o.into(),
        attached_by: by.into(),
        level: lvl,
        detached_at_ns: None,
    }
}

pub fn run() -> R<()> {
    println!();
    println!("## 6. Operation Context — one event, many operations; one event, none");
    println!();
    let mut s = ContextStore::new();
    s.add_operation(Operation {
        id: "op-edit".into(),
        session_id: Some("sess-1".into()),
        started_ns: TS0,
        ended_ns: None,
        label: "edit parser".into(),
    });
    s.add_operation(Operation {
        id: "op-recover".into(),
        session_id: Some("sess-1".into()),
        started_ns: TS0 + 600_000_000_000,
        ended_ns: None,
        label: "recover lost work".into(),
    });
    s.attach(attach("ev-1", "op-edit", "R6", Level::Weak));
    s.attach(attach("ev-2", "op-edit", "R5", Level::Probable));
    // The decisive row: the same event is in both operations.
    s.attach(attach("ev-2", "op-recover", "R4", Level::Strong));

    println!("| query | result |");
    println!("|---|---|");
    println!("| operations of ev-1 | {} |", join_ops(&s, "ev-1"));
    println!("| operations of ev-2 | {} |", join_ops(&s, "ev-2"));
    println!(
        "| events of op-edit | {} |",
        s.events_of("op-edit").join(", ")
    );
    println!(
        "| events of op-recover | {} |",
        s.events_of("op-recover").join(", ")
    );
    let all = vec!["ev-1".to_string(), "ev-2".to_string(), "ev-3".to_string()];
    println!(
        "| events belonging to no operation | {} |",
        s.orphan_events(&all)
            .iter()
            .map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    println!();
    println!("### 6.1 Retraction keeps history");
    println!();
    s.detach("ev-2", "op-recover", TS0 + 700_000_000_000);
    println!("after retracting ev-2 from op-recover:");
    println!("| query | result |");
    println!("|---|---|");
    println!("| operations of ev-2 | {} |", join_ops(&s, "ev-2"));
    println!(
        "| events of op-recover | `{}` (empty, the operation still exists) |",
        s.events_of("op-recover").join(", ")
    );
    println!("| membership rows retained | {} |", s.memberships_count());

    println!();
    println!("### 6.2 Why a single `events.operation_id` column cannot work");
    println!();
    println!("| requirement | single FK column | join table |");
    println!("|---|---|---|");
    println!("| ev-2 in two operations | impossible without duplicating the event | one row per (event, operation) |");
    println!("| ev-3 in no operation | forces a synthetic \"misc\" operation, destroying the distinction between \"belongs to nothing\" and \"not ingested\" | simply absent from the join table |");
    println!("| per-membership credibility | one level for the event, so a Strong recovery link forces the edit link to Strong too | level is per membership |");
    println!("| audit trail for a wrong link | no place to record which rule attached it | `attached_by` + `detached_mono_ns` |");
    println!("| retraction | UPDATE the column, losing the fact that a link ever existed | set `detached_mono_ns` |");
    println!();
    println!("This is the schema in migrations/0001_init.sql (`event_operations`), and the");
    println!("uniqueness constraint is on (event_id, operation_id), not on event_id.");
    Ok(())
}

fn join_ops(s: &ContextStore, event: &str) -> String {
    let ops: Vec<&str> = s
        .operations_of(event)
        .iter()
        .map(|o| o.id.as_str())
        .collect();
    if ops.is_empty() {
        "none".to_string()
    } else {
        ops.join(", ")
    }
}
