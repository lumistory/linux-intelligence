//! Experiment 2: reordering an out-of-order stream by `sequence` versus by
//! `ts_mono_ns`, and how to size the tolerance window.

use corr0h::envelope::{Event, Source};
use corr0h::ordering::{order_agreement, ReorderPolicy, SeqReorderer};
use corr0h::util::Rng;

use crate::common::{ev, R, TS0};

fn session_stream(n: u64, spacing_ns: u64, tick_quantised: bool, seed: u64) -> Vec<Event> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(n as usize);
    for s in 0..n {
        let mut ts = TS0 + s * spacing_ns + rng.below(spacing_ns / 4 + 1);
        if tick_quantised {
            ts -= ts % 10_000_000;
        }
        out.push(ev(
            &format!("e{s:05}"),
            Source::ShellBash,
            "s1",
            s,
            ts,
            "CommandStarted",
            serde_json::json!({"command": format!("cmd{s}")}),
        ));
    }
    out
}

/// Shuffle a sliding window of `depth`: exactly the shape a bounded reorder
/// buffer must repair.
fn disorder(stream: &[Event], depth: usize) -> Vec<(Event, u64)> {
    let mut rng = Rng::new(0xBEEF);
    let mut out = Vec::with_capacity(stream.len());
    let mut i = 0usize;
    let mut arrival = 0u64;
    while i < stream.len() {
        let end = (i + depth).min(stream.len());
        let mut chunk: Vec<Event> = stream[i..end].to_vec();
        for j in (1..chunk.len()).rev() {
            let k = rng.below(j as u64 + 1) as usize;
            chunk.swap(j, k);
        }
        for e in chunk {
            arrival += 100_000;
            out.push((e, arrival));
        }
        i = end;
    }
    out
}

fn ids(v: &[Event]) -> Vec<String> {
    v.iter().map(|e| e.event_id.clone()).collect()
}

fn run_reorder(arrived: &[(Event, u64)], grace_ns: u64, max_hold: usize) -> (Vec<Event>, corr0h::ordering::ReorderStats) {
    let mut r = SeqReorderer::new(ReorderPolicy { max_hold, grace_ns });
    let mut flushed: Vec<Event> = Vec::new();
    let mut now = 0u64;
    for (e, at) in arrived {
        now = now.max(*at) + 100_000;
        flushed.extend(r.advance(now).into_iter().map(|x| x.event));
        flushed.extend(r.push(e.clone(), *at).into_iter().map(|x| x.event));
    }
    now += 100_000_000;
    flushed.extend(r.advance(now).into_iter().map(|x| x.event));
    flushed.extend(r.finish().into_iter().map(|x| x.event));
    (flushed, r.stats())
}

pub fn run() -> R<()> {
    println!();
    println!("## 2. Out-of-order streams (SYNTHETIC, single session, 500 events, 1 ms apart)");
    for (idx, quantised) in [(1, false), (2, true)] {
        let label = if quantised { "timestamps quantised to the 10 ms CLK_TCK grid" } else { "distinct timestamps" };
        let truth = session_stream(500, 1_000_000, quantised, 7);
        let true_order = ids(&truth);
        println!();
        println!("### 2.{idx} {label}");
        println!();
        println!("| disorder depth | received | ts-sort | sequence | forced gaps | max buffered |");
        println!("|---|---|---|---|---|---|");
        for depth in [1usize, 2, 4, 8, 16, 32] {
            let arrived = disorder(&truth, depth);
            let received: Vec<Event> = arrived.iter().map(|(e, _)| e.clone()).collect();
            let rec = order_agreement(&true_order, &ids(&received));
            let mut by_ts = received.clone();
            by_ts.sort_by_key(|e| e.ts_mono_ns);
            let ts = order_agreement(&true_order, &ids(&by_ts));
            let (out, st) = run_reorder(&arrived, 8_000_000, 64);
            let seq = order_agreement(&true_order, &ids(&out));
            println!("| {depth} | {rec:.4} | {ts:.4} | {seq:.4} | {} | {} |",
                st.forced_gaps, st.max_hold_observed);
        }
    }

    println!();
    println!("### 2.3 Tolerance-window sizing (depth 8, quantised timestamps)");
    println!();
    let truth = session_stream(500, 1_000_000, true, 11);
    let true_order = ids(&truth);
    let arrived = disorder(&truth, 8);
    let pos: std::collections::HashMap<&str, usize> = arrived
        .iter()
        .enumerate()
        .map(|(i, (e, _))| (e.event_id.as_str(), i))
        .collect();
    let mut delays: Vec<u64> = Vec::new();
    for w in truth.windows(2) {
        if let (Some(&a), Some(&b)) = (pos.get(w[0].event_id.as_str()), pos.get(w[1].event_id.as_str())) {
            if b < a {
                delays.push((a - b) as u64 * 100_000);
            }
        }
    }
    delays.sort_unstable();
    let pick = |p: usize| delays.get(delays.len() * p / 100).copied().unwrap_or(0);
    println!("reorder delay distribution: n={} p50={} ns p90={} ns p99={} ns max={} ns",
        delays.len(), pick(50), pick(90), pick(99), delays.last().copied().unwrap_or(0));
    println!();
    println!("`grace_ns` is measured on the stream clock, so it also fires on a quiet");
    println!("session. A grace keyed on *more events arriving* would never fire exactly when");
    println!("a lost event sits alone in the buffer.");
    println!();
    println!("| grace | forced gaps | max buffered | agreement |");
    println!("|---|---|---|---|");
    for grace_us in [1u64, 2, 5, 10, 20, 50, 100, 250, 1000] {
        let (out, st) = run_reorder(&arrived, grace_us * 1_000, 64);
        let agree = order_agreement(&true_order, &ids(&out));
        println!("| {grace_us} us | {} | {} | {agree:.4} |", st.forced_gaps, st.max_hold_observed);
    }

    println!();
    println!("### 2.4 Why timestamps alone cannot be the answer");
    println!();
    let quantised_stream = session_stream(500, 1_000_000, true, 11);
    let mut ties = 0;
    for w in quantised_stream.windows(2) {
        if w[0].ts_mono_ns == w[1].ts_mono_ns {
            ties += 1;
        }
    }
    println!("With 10 ms quantisation and 1 ms spacing, {ties} of 499 adjacent pairs share a");
    println!("tick, so `ORDER BY ts_mono_ns` leaves their order to the sort's tie-break,");
    println!("not to the data. `sequence` has no ties by construction (0-C: gaps are exact).");
    println!("Cross-source ordering has no `sequence` space, so it must fall back to");
    println!("`ts_mono_ns` with an explicit +/-1 tick uncertainty that is stored, not hidden.");
    Ok(())
}
