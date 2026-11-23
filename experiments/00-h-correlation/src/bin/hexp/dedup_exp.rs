//! Experiment 1: event-level and semantic deduplication, scored against ground
//! truth on a synthetic 1000-event population.

use corr0h::dedup::{
    Decision, Deduplicator, FingerprintPolicy, Labelled, Score, Truth, WindowMode,
};
use corr0h::envelope::Source;
use corr0h::util::Rng;

use crate::common::{ev, R, TS0};

const CMDS: &[&str] = &[
    "ls -l /tmp",
    "git status",
    "cargo build",
    "make -j8",
    "vim notes.md",
    "python3 train.py",
    "docker ps",
    "ps aux",
    "grep -r TODO .",
    "curl https://example.com",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trap {
    /// One of the 500 origins. Its (type_class, session, subject) is unique
    /// inside any window used below, so calling it a duplicate is a real error.
    Origin,
    /// Same command text, same session, inside the window: a human running the
    /// same command twice, or a genuinely duplicated report. **Indistinguishable
    /// by content** — counted separately, never as a false positive.
    IndistinguishableDoubleTap,
    /// Same command text, different session, inside the window.
    OtherSessionSameWindow,
    /// Same command text, far in time.
    FarInTime,
    /// Different command text, inside the window.
    OtherCmdSameWindow,
    /// Same session, same command text, inside the window, but a **different
    /// `(pid, starttime)`**: two concurrent runs of one script. Decidable from
    /// the process key, which the specified fingerprint does not include — so
    /// this class is where the fingerprint genuinely over-merges.
    SameCmdDifferentProcess,
    /// A duplicate (either kind): merging it is correct.
    Duplicate,
}

impl Trap {
    fn as_str(self) -> &'static str {
        match self {
            Trap::Origin => "origin (decidable)",
            Trap::IndistinguishableDoubleTap => "D1 same cmd+session+window (UNDECIDABLE)",
            Trap::OtherSessionSameWindow => "D2 same cmd, other session, same window",
            Trap::FarInTime => "D3 same cmd, far in time",
            Trap::OtherCmdSameWindow => "D4 other cmd, same window",
            Trap::SameCmdDifferentProcess => "D5 same cmd+session+window, different pid:starttime",
            Trap::Duplicate => "(duplicate: merging is correct)",
        }
    }

    /// Negatives where a merge is a genuine mistake.
    fn is_decidable_negative(self) -> bool {
        matches!(
            self,
            Trap::Origin
                | Trap::OtherSessionSameWindow
                | Trap::FarInTime
                | Trap::OtherCmdSameWindow
                | Trap::SameCmdDifferentProcess
        )
    }
}

struct Population {
    events: Vec<Labelled>,
    trap: Vec<Trap>,
}

fn build_population(seed: u64) -> Population {
    let mut rng = Rng::new(seed);
    let mut events = Vec::with_capacity(1000);
    let mut traps = Vec::with_capacity(1000);
    let mut next_id = 0u64;
    let mut t = TS0;

    for k in 0..500u32 {
        next_id += 1;
        let id = format!("uuid-{next_id:04x}");
        let session = format!("sess-{}", k % 8);
        let ci = rng.below(CMDS.len() as u64) as usize;
        // Every origin gets a command line that is unique *within the whole
        // run*: a serial argument, which is what a human script does anyway.
        // Without this, "distinct" events would be physically indistinguishable
        // from duplicates and the precision number would measure the generator.
        let cmd: &str = &format!("{} --job {}", CMDS[ci], k);
        let source = match k % 3 {
            0 => Source::ShellBash,
            1 => Source::Proc,
            _ => Source::Fs,
        };
        let pid = 1000 + (k % 40);
        let starttime = 50_000 + u64::from(k % 40);
        let mk_payload = |c: &str| {
            serde_json::json!({
                "command": c, "pid": pid, "starttime": starttime, "cwd": "/home/u",
            })
        };
        events.push(Labelled {
            event: ev(
                &id,
                source,
                &session,
                u64::from(k),
                t,
                "CommandFinished",
                mk_payload(cmd),
            ),
            truth: Truth::Origin,
            origin: None,
        });
        traps.push(Trap::Origin);

        // 150 resends: identical id.
        if k % 10 < 3 {
            events.push(Labelled {
                event: ev(
                    &id,
                    source,
                    &session,
                    u64::from(k),
                    t,
                    "CommandFinished",
                    mk_payload(cmd),
                ),
                truth: Truth::ResendSameId,
                origin: Some(id.clone()),
            });
            traps.push(Trap::Duplicate);
        }

        // 150 semantic duplicates: fresh id, other source, 0..30 ms later.
        if (3..6).contains(&(k % 10)) {
            next_id += 1;
            let dup_id = format!("uuid-{next_id:04x}");
            let (other, ty) = if source == Source::Proc {
                (Source::ShellBash, "CommandFinished")
            } else {
                (Source::Proc, "ProcessExit")
            };
            let jitter = rng.below(30_000_000);
            events.push(Labelled {
                event: ev(
                    &dup_id,
                    other,
                    &session,
                    u64::from(k),
                    t + jitter,
                    ty,
                    serde_json::json!({"command": cmd, "pid": pid, "starttime": starttime}),
                ),
                truth: Truth::SameFactNewId,
                origin: Some(id.clone()),
            });
            traps.push(Trap::Duplicate);
        }

        // 250 near-miss negatives, 50 per class over 5 classes.
        if k % 2 == 0 {
            next_id += 1;
            let id2 = format!("uuid-{next_id:04x}");
            let other_cmd: &str = &format!("{} --job {}-alt", CMDS[(ci + 3) % CMDS.len()], k);
            let (trap, sess2, cmd2, dt, pid2, start2) = match (k / 2) % 5 {
                0 => (
                    Trap::IndistinguishableDoubleTap,
                    session.clone(),
                    cmd,
                    5_000_000u64,
                    pid,
                    starttime,
                ),
                1 => (
                    Trap::OtherSessionSameWindow,
                    format!("sess-{}", (k + 3) % 8),
                    cmd,
                    3_000_000,
                    pid,
                    starttime,
                ),
                2 => (
                    Trap::FarInTime,
                    format!("{session}-far"),
                    cmd,
                    30_000_000_000,
                    pid,
                    starttime,
                ),
                3 => (
                    Trap::SameCmdDifferentProcess,
                    session.clone(),
                    cmd,
                    2_000_000,
                    pid + 500,
                    starttime + 1,
                ),
                _ => (
                    Trap::OtherCmdSameWindow,
                    session.clone(),
                    other_cmd,
                    1_000_000,
                    pid + 1,
                    starttime,
                ),
            };
            events.push(Labelled {
                event: ev(
                    &id2,
                    Source::ShellBash,
                    &sess2,
                    u64::from(k) + 1,
                    t + dt,
                    "CommandFinished",
                    serde_json::json!({"command": cmd2, "pid": pid2, "starttime": start2}),
                ),
                truth: Truth::Distinct,
                origin: None,
            });
            traps.push(trap);
        }

        t += 20_000_000 + rng.below(5_000_000);
    }
    Population {
        events,
        trap: traps,
    }
}

fn exact_only(pop: &Population) -> (Vec<Decision>, Score) {
    let mut seen = std::collections::HashSet::new();
    let decisions: Vec<Decision> = pop
        .events
        .iter()
        .map(|l| {
            if seen.insert(l.event.event_id.clone()) {
                Decision::Fresh
            } else {
                Decision::DuplicateEventId
            }
        })
        .collect();
    let s = corr0h::dedup::score(&pop.events, &decisions);
    (decisions, s)
}

fn fingerprint(pop: &Population, policy: FingerprintPolicy) -> (Vec<Decision>, Score) {
    let mut d = Deduplicator::new(policy);
    let decisions: Vec<Decision> = pop.events.iter().map(|l| d.offer(&l.event)).collect();
    let s = corr0h::dedup::score(&pop.events, &decisions);
    (decisions, s)
}

/// Score restricted to *decidable* negatives: an undecidable double-tap is not
/// counted as a false positive, and is reported as its own column.
fn decidable_score(pop: &Population, decisions: &[Decision]) -> (usize, usize, usize) {
    let mut tp = 0;
    let mut fp = 0;
    let mut decidable_neg = 0;
    for (i, l) in pop.events.iter().enumerate() {
        let called_dup = !matches!(decisions[i], Decision::Fresh);
        match l.truth {
            Truth::ResendSameId | Truth::SameFactNewId => {
                if called_dup {
                    tp += 1;
                }
            }
            Truth::Origin | Truth::Distinct => {
                if pop.trap[i].is_decidable_negative() {
                    decidable_neg += 1;
                    if called_dup {
                        fp += 1;
                    }
                }
            }
        }
    }
    (tp, fp, decidable_neg)
}

fn undecidable_merged(pop: &Population, decisions: &[Decision]) -> (usize, usize) {
    let mut merged = 0;
    let mut total = 0;
    for (i, _) in pop.events.iter().enumerate() {
        if pop.trap[i] == Trap::IndistinguishableDoubleTap {
            total += 1;
            if !matches!(decisions[i], Decision::Fresh) {
                merged += 1;
            }
        }
    }
    (merged, total)
}

fn fn_rate(s: &Score) -> f64 {
    let d = s.true_pos + s.false_neg;
    if d == 0 {
        0.0
    } else {
        s.false_neg as f64 / d as f64
    }
}

pub fn run() -> R<()> {
    println!("## 1. Deduplication (SYNTHETIC streams, ground truth by construction)");
    let pop = build_population(0xC0_11_EA);
    let dup_count = pop
        .events
        .iter()
        .filter(|l| matches!(l.truth, Truth::ResendSameId | Truth::SameFactNewId))
        .count();
    let neg_count = pop
        .events
        .iter()
        .filter(|l| l.truth == Truth::Distinct)
        .count();
    println!();
    println!(
        "population: {} events = 500 origins + {} duplicates + {} near-miss negatives",
        pop.events.len(),
        dup_count,
        neg_count
    );

    println!();
    println!("### 1.1 Scheme A — exact `event_id` only (baseline)");
    println!();
    println!("| metric | value |");
    println!("|---|---|");
    let (_, exact) = exact_only(&pop);
    println!("| duplicates found (TP) | {} |", exact.true_pos);
    println!("| false merges (FP) | {} |", exact.false_pos);
    println!("| missed duplicates (FN) | {} |", exact.false_neg);
    println!("| precision | {:.4} |", exact.precision());
    println!("| recall | {:.4} |", exact.recall());
    println!(
        "| false-merge rate FP/(TP+FP) | {:.4} |",
        exact.false_merge_rate()
    );
    println!(
        "| undetected-duplicate rate FN/(TP+FN) | {:.4} |",
        fn_rate(&exact)
    );

    println!();
    println!("Ground truth is built in, and the population contains one class that is");
    println!("**undecidable by content** (D1: same command text, same session, inside the");
    println!("window — a human double-tap or a genuine double-report). D1 merges are");
    println!("reported in their own column and are NOT counted as false positives, because");
    println!("no content-based scheme can separate them. Every other negative is decidable.");

    println!();
    println!("### 1.2 Scheme B — semantic fingerprint, parameter sweep");
    println!();
    println!("key = `type_class | session? | subject | ts_bucket`; `session?` is `-`");
    println!("unless `require_same_session`. Every row is a full re-run of the stream.");
    println!("precision is TP/(TP+decidable FP); `D1 merged` is the irreducible residue.");
    println!();
    println!("| window | mode | key | TP | FP (decidable) | FN | precision | recall | false-merge | D1 merged |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for window_ms in [10u64, 50, 100, 250, 500, 1000, 2000] {
        for mode in [WindowMode::Single, WindowMode::Triple] {
            for (label, sess, proc_key) in [
                ("no session", false, false),
                ("+session", true, false),
                ("+session+process", true, true),
            ] {
                let policy = FingerprintPolicy {
                    window_ns: window_ms * 1_000_000,
                    mode,
                    require_same_session: sess,
                    require_same_process: proc_key,
                };
                let (decisions, s) = fingerprint(&pop, policy);
                let (tp, fp, _neg) = decidable_score(&pop, &decisions);
                let (d1m, d1t) = undecidable_merged(&pop, &decisions);
                let prec = if tp + fp == 0 {
                    1.0
                } else {
                    tp as f64 / (tp + fp) as f64
                };
                let fmr = if tp + fp == 0 {
                    0.0
                } else {
                    fp as f64 / (tp + fp) as f64
                };
                println!(
                    "| {window_ms} ms | {} | {label} | {tp} | {fp} | {} | {prec:.4} | {:.4} | {fmr:.4} | {d1m}/{d1t} |",
                    if mode == WindowMode::Single { "single" } else { "triple" },
                    s.false_neg, s.recall()
                );
            }
        }
    }

    println!();
    println!("### 1.3 False merges by trap class (chosen: 200 ms triple, three key variants)");
    println!();
    let chosen = FingerprintPolicy {
        window_ns: 200_000_000,
        mode: WindowMode::Triple,
        require_same_session: true,
        require_same_process: false,
    };
    let (dec_session, s_session) = fingerprint(&pop, chosen);
    let (dec_nosession, s_nosession) = fingerprint(
        &pop,
        FingerprintPolicy {
            require_same_session: false,
            ..chosen
        },
    );
    let (dec_proc, s_proc) = fingerprint(
        &pop,
        FingerprintPolicy {
            require_same_process: true,
            ..chosen
        },
    );
    println!("| trap class | n | key: no session | key: +session | key: +session+process |");
    println!("|---|---|---|---|---|");
    for trap in [
        Trap::IndistinguishableDoubleTap,
        Trap::OtherSessionSameWindow,
        Trap::FarInTime,
        Trap::OtherCmdSameWindow,
        Trap::SameCmdDifferentProcess,
        Trap::Origin,
    ] {
        let mut total = 0;
        let mut fp_s = 0;
        let mut fp_n = 0;
        let mut fp_p = 0;
        for (i, t) in pop.trap.iter().enumerate() {
            if *t != trap {
                continue;
            }
            total += 1;
            if !matches!(dec_session[i], Decision::Fresh) {
                fp_s += 1;
            }
            if !matches!(dec_nosession[i], Decision::Fresh) {
                fp_n += 1;
            }
            if !matches!(dec_proc[i], Decision::Fresh) {
                fp_p += 1;
            }
        }
        println!(
            "| {} | {} | {} | {} | {} |",
            trap.as_str(),
            total,
            fp_n,
            fp_s,
            fp_p
        );
    }
    println!();
    println!(
        "no session    : TP {} FP {} FN {} precision {:.4} recall {:.4}",
        s_nosession.true_pos,
        s_nosession.false_pos,
        s_nosession.false_neg,
        s_nosession.precision(),
        s_nosession.recall()
    );
    println!(
        "+session      : TP {} FP {} FN {} precision {:.4} recall {:.4}",
        s_session.true_pos,
        s_session.false_pos,
        s_session.false_neg,
        s_session.precision(),
        s_session.recall()
    );
    println!(
        "+session+proc : TP {} FP {} FN {} precision {:.4} recall {:.4}",
        s_proc.true_pos,
        s_proc.false_pos,
        s_proc.false_neg,
        s_proc.precision(),
        s_proc.recall()
    );
    println!();
    println!("Dropping the session from the key is what turns a safe fingerprint into a");
    println!("dangerous one: every D2 row becomes a merge, and D2 is decidable. Adding the");
    println!("process key fixes D5 at no cost to recall **in this population**, because");
    println!("cross-source duplicates carry the same (pid,starttime); it breaks the moment a");
    println!("hook reports no starttime, which is why the flag is a policy choice and not a");
    println!("constant.");

    println!();
    println!("### 1.4 Stability across three seeds (chosen config)");
    println!();
    println!("`precision (decidable)` excludes the D1 double-tap class; `precision (all)` counts");
    println!("it as a false positive.");
    println!();
    println!("| seed | events | exact recall | precision (decidable) | precision (all) | fingerprint recall |");
    println!("|---|---|---|---|---|---|");
    for seed in [0xC0_11_EAu64, 0x5E_C0_11, 0x00_11_EA] {
        let p = build_population(seed);
        let (_, se) = exact_only(&p);
        let (dec, sf) = fingerprint(&p, chosen);
        let (tp, fp, _) = decidable_score(&p, &dec);
        let dec_prec = if tp + fp == 0 {
            1.0
        } else {
            tp as f64 / (tp + fp) as f64
        };
        println!(
            "| {seed:#x} | {} | {:.4} | {dec_prec:.4} | {:.4} | {:.4} |",
            p.events.len(),
            se.recall(),
            sf.precision(),
            sf.recall()
        );
    }

    println!();
    println!("### 1.5 Fingerprint blind spot: same fact, different subject text");
    println!();
    let mut d = Deduplicator::new(chosen);
    let a = ev(
        "x1",
        Source::ShellBash,
        "s1",
        1,
        TS0,
        "CommandFinished",
        serde_json::json!({"command": "make -j8"}),
    );
    let b = ev(
        "x2",
        Source::Proc,
        "s1",
        2,
        TS0 + 1_000_000,
        "ProcessExit",
        serde_json::json!({"command": "/usr/bin/make -j8"}),
    );
    println!("hook `make -j8`           -> {:?}", d.offer(&a));
    println!("proc `/usr/bin/make -j8`  -> {:?}", d.offer(&b));
    println!("The subject is normalized command text; a `/proc/<pid>/cmdline` absolute");
    println!("path does not hash equal to the typed command. Measured miss, not a bug to");
    println!("hide: basename-stripping is a Phase 1 decision, not a Phase 0 result.");

    println!();
    println!("### 1.6 Verdict");
    println!();
    println!(
        "Exact `event_id` is necessary and free, but recall {:.4} on a population where",
        exact.recall()
    );
    println!(
        "{:.1}% of all duplicates carry a fresh id. The fingerprint is what recovers them,",
        (1.0 - exact.recall()) * 100.0
    );
    println!(
        "and it costs {:.4} false merges on the chosen config.",
        s_session.false_merge_rate()
    );
    Ok(())
}
