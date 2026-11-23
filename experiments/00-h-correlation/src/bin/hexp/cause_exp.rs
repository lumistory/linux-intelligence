//! Experiment 5: time adjacency is not causation — counterexamples plus the API
//! constraints that keep the lie out of the interface.

use corr0h::confidence::Level;
use corr0h::relations::{validate, Counterexample, Rejection, Relation, RelationKind};

use crate::common::R;

pub fn run() -> R<()> {
    println!();
    println!("## 5. \"Time adjacency\" is not \"cause\"");

    println!();
    println!("### 5.1 Four counterexamples, each observed as a shape rather than argued");
    println!();
    println!("| counterexample | naive claim | why it is false | defensible relation | level |");
    println!("|---|---|---|---|---|");
    let whys = [
        "the hook fires at command exit; children keep writing for seconds afterwards \
         (`make -j` is the canonical case)",
        "`echo x > f` returns after write(2); writeback lands later, so the file event can \
         postdate the process exit",
        "vim writes a temp file and rename(2)s it over the target; the modified path is the \
         temp path, and the target only appears as a rename destination",
        "inotify carries no pid (0-E), so a file two processes appended to cannot be \
         attributed to either one",
    ];
    for (c, why) in [
        Counterexample::BackgroundTask,
        Counterexample::DelayedWrite,
        Counterexample::EditorRenameSave,
        Counterexample::TwoWritersOneFile,
    ]
    .iter()
    .zip(whys)
    {
        let (k, l) = c.defensible();
        println!("| {} | \"{} caused the file change\" | {} | `{}` | {} |",
            c.as_str(), c.as_str(), why, k.as_str(), l.as_str());
    }

    println!();
    println!("### 5.2 API constraint: three kinds, no way to spell a cause");
    println!();
    println!("| kind string | parses | directional |");
    println!("|---|---|---|");
    for k in ["observed_within", "spawned_by", "modified_during", "caused_by", "causes", "led_to", "result_of"] {
        match RelationKind::parse(k) {
            Some(kind) => println!("| `{k}` | yes | {} |", kind.is_directional()),
            None => println!("| `{k}` | REJECTED | - |"),
        }
    }
    println!();
    println!("Enforced twice: `RelationKind::parse` returns None, and the schema CHECK in");
    println!("migrations/0001_init.sql rejects the row. The unit test");
    println!("`caused_by_is_rejected_by_the_schema` asserts the INSERT fails.");

    println!();
    println!("### 5.3 Validation rejects the two illegal shapes");
    println!();
    println!("| claim | fork edge | observed input | result |");
    println!("|---|---|---|---|");
    for (kind, edge, observed, label) in [
        (RelationKind::SpawnedBy, false, true, "spawned_by with no fork edge"),
        (RelationKind::ObservedWithin, false, false, "Confirmed from inferred inputs only"),
        (RelationKind::ModifiedDuring, false, true, "modified_during with a real file event"),
        (RelationKind::SpawnedBy, true, true, "spawned_by with a kernel fork edge"),
    ] {
        let r = validate(kind, Level::Confirmed, edge, observed);
        let text = match r {
            Ok(()) => "accepted".to_string(),
            Err(Rejection::NoForkEvidence) => "REJECTED NoForkEvidence".to_string(),
            Err(Rejection::InferredCannotConfirm) => "REJECTED InferredCannotConfirm".to_string(),
            Err(Rejection::NotCausal) => "REJECTED NotCausal".to_string(),
        };
        println!("| {label} | {edge} | {observed} | {text} |");
    }

    println!();
    println!("### 5.4 Rendering is capped by kind, not by level");
    println!();
    let causal = ["caused", "because", "triggered", "result of", "wrote", "produced"];
    for kind in [RelationKind::ObservedWithin, RelationKind::ModifiedDuring, RelationKind::SpawnedBy] {
        let r = Relation::new(kind, Level::Confirmed, "R3");
        let text = r.render("cmd:npm test", "./dist/bundle.js");
        let dirty: Vec<&str> = causal.iter().copied().filter(|w| text.contains(w)).collect();
        println!("- `{}` at Confirmed -> \"{}\"{}", kind.as_str(), text,
            if dirty.is_empty() { " (no causal verb)".to_string() } else { format!(" (CAUSAL: {dirty:?})") });
    }
    println!();
    println!("`modified_during` renders \"was modified during\", never \"was modified by\", even at");
    println!("Confirmed. The kind caps the verb; the level only caps confidence in the claim");
    println!("that is actually being made.");

    println!();
    println!("### 5.5 The one relation with a real direction");
    println!();
    println!("`spawned_by` is directional because the kernel reported a parent edge at fork");
    println!("time (`ppid` at the moment the child was created, walked over");
    println!("/proc/<pid>/stat`). It is still not causation: a parent that spawns `sh -c");
    println!("\"rm -rf build\"` did not itself compute anything. It is the *strongest* relation");
    println!("Phase 0 supports, and it is a containment relation, not a causal one.");
    Ok(())
}
