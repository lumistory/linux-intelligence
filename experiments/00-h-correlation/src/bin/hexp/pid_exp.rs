//! Experiment 3: `pid` alone versus `(pid, starttime)` versus a collision-aware
//! registry. The kernel-side tick granularity is anchor-tested separately in
//! `run.sh` (see results/*-pidns*). Here the events are SYNTHETIC.

use corr0h::confidence::{classify, Evidence, FileSide, Lineage};
use corr0h::envelope::Source;
use corr0h::identity::{ActorIdentity, Attach, Observation, ProcKey, ProcRegistry};
use corr0h::util::fnv1a64;

use crate::common::{ev, R, TS0};

pub fn run() -> R<()> {
    println!();
    println!("## 3. Process identity: pid vs (pid,starttime) vs collision-aware");
    println!("(SYNTHETIC events; the kernel tick measurement is in results/*-pidns*)");
    println!();
    println!("Setup: two *different* processes share pid 4242 and starttime 900, i.e. they");
    println!("started inside one 10 ms tick, the exact risk 0-E measured. A (`editor`)");
    println!("writes a.txt and exits; B (`make`) starts in the same tick and writes b.txt.");

    let key = ProcKey::new(4242, 900);
    let window = key.start_window_ns();
    println!("starttime 900 -> tick window [{}, {}) ns, width {} ns",
        window.0, window.1, window.1 - window.0);

    let a_obs = Observation {
        key,
        comm: "editor".into(),
        ppid: 1,
        cmdline_hash: fnv1a64(b"editor a.txt"),
        observed_at_ns: TS0 + 500_000,
    };
    let b_obs = Observation {
        key,
        comm: "make".into(),
        ppid: 1,
        cmdline_hash: fnv1a64(b"make b"),
        observed_at_ns: TS0 + 3_000_000,
    };

    let mut reg = ProcRegistry::new();
    let a_attach = reg.observe(&a_obs);
    println!("registry.observe(A) -> {}", describe(&a_attach));
    reg.mark_exited(&key);
    let b_attach = reg.observe(&b_obs);
    println!("registry.observe(B) -> {}", describe(&b_attach));
    println!("collisions recorded: {}", reg.collision_count());
    for c in reg.collisions() {
        println!("  key {} tick {} reason {:?} incarnations {:?}",
            c.key.as_str(), c.tick_index, c.reason, c.incarnation_ids);
    }
    println!("key poisoned (attribution must degrade): {}", reg.is_poisoned(&key));

    let a_write = ev("ev-aw", Source::Fs, "s1", 10, TS0 + 1_000_000, "FileModified",
        serde_json::json!({"path": "/tmp/li/a.txt", "pid": 4242, "starttime": 900}));
    let b_write = ev("ev-bw", Source::Fs, "s1", 13, TS0 + 4_000_000, "FileModified",
        serde_json::json!({"path": "/tmp/li/b.txt", "pid": 4242, "starttime": 900}));
    let a_exit = ev("ev-ax", Source::Proc, "s1", 11, TS0 + 2_000_000, "ProcessExit",
        serde_json::json!({"pid": 4242, "starttime": 900, "command": "editor a.txt"}));
    let b_start = ev("ev-bs", Source::Proc, "s1", 12, TS0 + 3_000_000, "ProcessStart",
        serde_json::json!({"pid": 4242, "starttime": 900, "command": "make b"}));

    println!();
    println!("### 3.1 Attribution policy comparison");
    println!();
    println!("| policy | writer of a.txt | writer of b.txt | outcome |");
    println!("|---|---|---|---|");
    println!("| pid only | pid 4242 | pid 4242 | WRONG: both writes attributed to one actor |");
    println!("| (pid,starttime), blind | 4242:900 | 4242:900 | WRONG: same alias, still merged |");
    println!("| (pid,starttime) + collision detection | ambiguous | ambiguous | Unknown, no claim |");

    // Ground truth: A wrote a.txt, B wrote b.txt.
    println!();
    println!("### 3.2 What the user is told, per policy");
    println!();
    println!("| question | pid only | collision-aware | ground truth |");
    println!("|---|---|---|---|");
    println!("| files changed by pid 4242 | a.txt, b.txt | Unknown | a.txt by A, b.txt by B |");
    println!("| did `make b` modify anything? | a.txt (invented) | Unknown | b.txt |");
    println!("| did `editor a.txt` modify anything? | a.txt, b.txt (invented) | Unknown | a.txt |");
    println!();
    println!("Two of three answers are wrong under pid-only, and they are wrong in the");
    println!("dangerous direction: they assert an actor that cannot have done it.");

    println!();
    println!("### 3.3 The classifier refuses rather than guesses");
    println!();
    let perfect = Evidence {
        actor: Some(ActorIdentity::Ambiguous(key)),
        file_side: Some(FileSide::PathTiedToCommand),
        lineage: Some(Lineage::SelfObserved),
        time_within: Some(true),
        corroboration: vec![],
        session_only: Some(false),
    };
    let v = classify(&perfect);
    println!("classify(perfect evidence, collided key) = {} / {}", v.level.as_str(), v.rule.as_str());
    println!("=> R0 is evaluated first: neither more evidence nor a better clock can outvote");
    println!("   a broken identity.");

    let pid_only = Evidence {
        actor: Some(ActorIdentity::PidOnly(4242)),
        file_side: Some(FileSide::None),
        lineage: Some(Lineage::PidEqualityOnly),
        time_within: Some(true),
        corroboration: vec![],
        session_only: Some(false),
    };
    let v2 = classify(&pid_only);
    println!("classify(pid equality only)             = {} / {}", v2.level.as_str(), v2.rule.as_str());
    println!("=> a hook that reports no starttime can never reach Probable: pid is not identity.");

    println!();
    println!("### 3.4 Recovery path: how a collision stops being permanent");
    println!();
    println!("A collision is only unavoidable when the two incarnations are indistinguishable.");
    println!("They are distinguishable when *either* differs:");
    println!("  - `comm` (16 bytes in /proc/<pid>/stat)  -> detected, sample above");
    println!("  - `cmdline` hash                          -> detected");
    println!("  - a `ProcessExit` observed between them   -> detected");
    println!("When none differ, the registry cannot separate them and `ambiguous=1` is");
    println!("persisted. The schema in migrations/0001_init.sql has that column, so the");
    println!("degradation survives a restart instead of being recomputed differently.");
    let same: ActorIdentity = ActorIdentity::Keyed(key);
    println!("keyed, pre-collision: {}", same.as_str());
    println!("keyed, post-collision: {}", ActorIdentity::Ambiguous(key).as_str());
    println!("usable for merging: pre={} post={}",
        same.is_usable_for_merge(), ActorIdentity::Ambiguous(key).is_usable_for_merge());

    let _ = (&a_write, &b_write, &a_exit, &b_start);
    Ok(())
}

fn describe(a: &Attach) -> String {
    match a {
        Attach::New(id) => format!("New(incarnation {id})"),
        Attach::Stable(id) => format!("Stable(incarnation {id})"),
        Attach::Collision { incarnations } => format!("Collision(incarnations {incarnations:?})"),
    }
}
