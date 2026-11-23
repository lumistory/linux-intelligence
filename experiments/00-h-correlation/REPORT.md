# 0-H — Deduplication, out-of-order streams, process identity, credibility,
#        causality language, Operation Context and secret redaction

Run of record: `results/hexp-full-20261123T141333Z.log` (459 lines, the whole
`hexp` battery in one run, exit 0), plus the per-section logs under
`results/20261123T141251Z-*.log`. Library: `corr0h` (rusqlite 0.32 against the
host's SQLite 3.53.4, deliberately **not** the bundled copy so the numbers
describe what production will run).

All experiment populations are **SYNTHETIC** and labelled as such in the output;
ground truth is built in by construction. Nothing here is a claim about real
traffic. Where a real-kernel fact was needed (pid reuse, `kill -9` recovery,
WAL concurrency) the measurement is on the real host and marked `observed`.

## 1. Objective

Four Phase 1 questions that a schema alone cannot answer:

1. Is `event_id` deduplication enough, and what does a *semantic* fingerprint
   add or cost?
2. How out-of-order can an event stream be before reordering by `sequence`
   stops working, and what does `ts_wall` alone get wrong?
3. Is `pid` usable as a process identity, and what should the system do when
   `(pid, starttime)` collides?
4. Can the five credibility levels be **checkable rules** rather than vibes, and
   can the API be made unable to say "caused"?

Plus two persistence questions: can one event belong to several Operation
Contexts, and does redaction-before-insert actually work.

## 2. Hypothesis

| # | Hypothesis | Verdict |
|---|---|---|
| H1 | `event_id` catches exact resends only; a semantic fingerprint adds recall but merges genuinely distinct events unless session *and* process identity are in the key. | **PASS** |
| H2 | `ts_wall` alone cannot order events that share a 10 ms tick; only `sequence` can. | **PASS** |
| H3 | `pid` alone mis-attributes across reuse; `(pid,starttime)` is better but collides within a tick; the correct behaviour is to refuse to attribute. | **PASS** |
| H4 | The five levels can be expressed as checkable predicates, and monotone under added corroboration. | **PASS** |
| H5 | A relation kind enum of `observed_within` / `spawned_by` / `modified_during` makes "caused" unrepresentable, and validation rejects illegal shapes. | **PASS** |
| H6 | An event can belong to many Operation Contexts and to none; a single FK column cannot express this. | **PASS** |
| H7 | A redactor can strip the three required secret shapes before insert with zero misses and few false positives. | **PASS (V2: 0 misses)** |

## 3. Method

| file | role |
|---|---|
| `src/envelope.rs` | the shared event envelope: `schema_version`, `event_id`, `source`, `session_id`, `sequence`, `ts_mono_ns`, `ts_wall`, `type`, `payload`, `evidence_status` |
| `src/identity.rs` | `pid` vs `(pid,starttime)` vs collision-aware process keys |
| `src/relations.rs` | the three permitted relation kinds, validation, rendering cap |
| `src/ordering.rs` | sequence-based reordering with a bounded tolerance window |
| `src/context.rs` | many-to-many Operation Context membership, retraction |
| `src/redact.rs` | V1/V2 secret redaction |
| `src/db.rs` | schema, migrations, WAL setup, busy handling, queries |
| `src/util.rs` | percentiles, timing |
| `src/bin/hexp/*.rs` | one module per experiment section |
| `src/bin/hdb.rs` | 100k-row benchmark and retention harness |
| `migrations/0001_init.sql` | events, sessions, processes, operations, event_operations, event_relations, evidence, checkpoints, recovery_plans, approvals, adapter_registry, schema_version |
| `migrations/0002_add_event_redaction.sql` | forward migration adding redaction metadata |

Machine: Arch Linux, kernel 7.2.8-zen, x86_64, `CLK_TCK=100`, rustc 1.99.0,
SQLite 3.53.4, 20 cores.

## 4. Raw results

### 4.1 Deduplication (`§1` of the run log)

Population: 1050 events = 500 origins + 300 duplicates + 250 near-miss
negatives, constructed so that every merge decision has a known answer.

Scheme A — exact `event_id` only:

| metric | value |
|---|---|
| true positives | 150 |
| false merges | 0 |
| missed duplicates | 150 |
| precision | 1.0000 |
| recall | **0.5000** |

So `event_id` is *safe but half-blind*: it never merges two distinct events, and
it never catches the same fact reported twice by two sources.

Scheme B — semantic fingerprint, chosen config (200 ms window, triple key
`type + session + process`), false merges by trap class:

| trap class | n | no session | +session | +session+process |
|---|---|---|---|---|
| D1 same cmd+session+window (**undecidable**) | 50 | 50 | 50 | 50 |
| D2 same cmd, other session, same window | 50 | 50 | 0 | 0 |
| D3 same cmd, far in time | 50 | 0 | 0 | 0 |
| D4 other cmd, same window | 50 | 0 | 0 | 0 |
| D5 same cmd+session+window, different pid:starttime | 50 | 50 | 50 | 0 |
| origin | 500 | 0 | 0 | 0 |

| key | precision | recall |
|---|---|---|
| no session | 0.6667 | 1.0000 |
| +session | 0.7500 | 1.0000 |
| +session+process | **0.8571** | 1.0000 |

The residual 50 false merges are exactly the D1 class, which is **undecidable by
content** — a human double-tap and a genuine double-report are the same bytes.
The report keeps them in their own column instead of counting them as errors.

### 4.2 Out-of-order streams (`§2`)

500 events 1 ms apart, shuffled:

| timestamp kind | reorder result |
|---|---|
| distinct (1 ms apart) | ts ordering recovers the true order |
| quantised to the 10 ms `CLK_TCK` grid | **ts ordering cannot recover it**; only `sequence` can |

Tolerance-window sizing at depth 8 with quantised timestamps is reported in
`§2.3`; the practical conclusion is that **`sequence` is the ordering key and
`ts_wall` is a display attribute**, not the other way round.

### 4.3 Process identity (`§3`)

| policy | behaviour on reuse | behaviour on same-tick collision |
|---|---|---|
| `pid` only | merges two different processes' events | merges |
| `(pid, starttime)` | separates reuse | **still merges** (10 ms granularity) |
| collision-aware | separates reuse | **refuses to attribute**, level drops to `Unknown` |

`§3.4` records how a collision stops being permanent (a later event with a
distinct starttime re-establishes identity).

### 4.4 Credibility — 24 scenarios (`§4`)

| level | count |
|---|---|
| Confirmed | 4 |
| Strong | 4 |
| Probable | 3 |
| Weak | 6 |
| Unknown | 7 |

All 24 assertions pass. `§4.2` verifies **monotonicity**: adding a corroborating
source never lowers the level, so a consumer cannot be surprised by a
re-classification that makes evidence weaker as more is learned.

### 4.5 "Time adjacency is not cause" (`§5`)

Four counterexamples, each measured as a shape rather than argued: a background
task, a delayed write, an editor's rename-save, and two processes writing one
file. The API constraint:

| permitted `relation.kind` | meaning |
|---|---|
| `observed_within` | A and B both observed inside interval I; no direction claimed |
| `spawned_by` | direct fork relationship with matching `(pid,starttime)` |
| `modified_during` | a file event inside a process's observed lifetime |

There is no `caused_by`. `§5.3` shows validation rejecting the two illegal
shapes (a causal claim with no direct evidence; a direction on
`observed_within`). `§5.5` records the single relation that *does* carry a real
direction: `spawned_by`.

### 4.6 Operation Context (`§6`)

One event belongs to several Operations and to none, both stored and queried
correctly. `§6.1` shows retraction keeping history (membership is withdrawn,
the row is not deleted). `§6.2` states the structural reason a single
`events.operation_id` column cannot work: it forces exactly one owner.

### 4.7 Secret redaction (`§7`)

Test corpus: 53 clean commands + 22 secret-bearing commands.

| version | false positives | misses |
|---|---|---|
| V1 | 1 | **4** |
| V2 | 1 | **0** |

The three required shapes, verbatim:

```
mysql -psecret          -> password replaced
export API_KEY=xxx      -> value replaced
curl -H "Authorization: Bearer xxx"  -> token replaced
```

`§7.5` records the residual risk: a secret passed as a **bare positional
argument** with nothing to key on cannot be detected by pattern. `§7.6` records
that the same secret twice produces the same 64-bit FNV-1a digest, which makes
the digest *linkable* — useful for "this credential was used again", and
explicitly not a key-derivation function.

### 4.8 Storage: 100 000 events (`results/20261123T141251Z-bench-100k.log`)

| measurement | value |
|---|---|
| insert 100 000 events | 518 ms (**5.18 µs/row**, 192 962 rows/s) |
| db size | 32 161 792 B (~30.7 MiB) |
| q1 events by session (p50 / p95 / p99) | 72.5 / 84.7 / 97.8 µs |
| q2 events by time range | 143.4 / 169.5 / 240.9 µs |
| q3 events by operation | 32.5 / 36.5 / 40.2 µs |
| q4 relations by path, **no index** | **2 753.9 / 3 504.1 / 4 032.3 µs** |
| q4b same query, expression index | **16.3 / 21.3 / 31.0 µs** |
| expression index build cost | 33 ms, +4.0 MiB |
| single-row upsert | 8.4 / 9.7 / 15.7 µs |

The 169× difference between q4 and q4b is the single most actionable number in
this experiment: any relation query keyed on a JSON-extracted path **must** have
an expression index, and adding it later costs a full table scan.

Also measured on the real host: WAL with 1 writer + 4 readers under
`busy_timeout`, `kill -9` recovery followed by `PRAGMA integrity_check`, and a
v1→v2 migration on a populated database (`results/20261123T141251Z-kill9*.log`,
`-wal-concurrency.log`, `-mig-*.log`).

## 5. Analysis

**Two dedup layers, not one.** `event_id` is the *delivery* dedup — it is about
retransmission and it must stay exact and free. The semantic fingerprint is the
*corroboration* layer: it says "these two reports are probably the same fact",
and it belongs in `event_relations` with a confidence level, never in a
`DELETE`. Scheme A's 0.50 recall is not a bug; it is the correct behaviour for a
layer that must never merge two different events.

**The session in the key is load-bearing.** Dropping it turns a 0.86-precision
fingerprint into a 0.67 one, and every one of those extra merges is *decidable*
(D2), i.e. a real error rather than an undecidable case. Adding the process key
buys another 0.10 by removing D5, but it only works while hooks report
`starttime` — which is why it is a configuration flag, not a constant.

**Ordering is `sequence`, timestamps are display.** At `CLK_TCK=100`, five
events can share a timestamp. Any design that sorts by `ts_wall` and calls the
result "the order things happened" is wrong at exactly the resolution where
causation claims would be made.

**Identity refusal is a feature.** The collision-aware policy does not guess; it
drops to `Unknown`. Given 20/20 same-tick collisions measured in 0-E and 0-G,
this is not a theoretical branch.

**Credibility is a contract, not a label.** The monotonicity check in `§4.2` is
the part that makes the level meaningful: if adding evidence could ever lower
it, no consumer could act on the level at all.

**"Caused" is unrepresentable, by construction.** Not "we avoid saying caused"
— the enum has no such variant and validation rejects hand-written shapes. This
is the cheapest possible enforcement of the project's core evidence principle.

**Redaction is pattern work, and it has a known hole.** V2 catches the three
required shapes with zero misses and one false positive in 53 clean commands.
A bare positional secret is undetectable by pattern; the honest answer is that
it is `NOT TESTED`-able, not that it is handled.

**The storage numbers change the schema.** 5.18 µs/row is fast enough that the
journal is not the bottleneck (0-C already showed the flush is). The q4/q4b gap
means relation lookups on paths need an expression index from day one.

## 6. Known limitations

**NOT TESTED / not covered:**

1. **All experiment populations are SYNTHETIC.** Dedup, ordering, identity and
   credibility numbers come from constructed streams with built-in ground truth.
   Real hook traffic may have trap classes nobody thought to construct.
2. **NOT TESTED: more than 100 000 events.** No measurement at 1 M or 10 M rows;
   the retention design is therefore a plan, not a validated behaviour.
3. **NOT TESTED: a long-running writer.** The bench inserts in one process; no
   multi-hour soak, no WAL growth measurement, no checkpointing pressure.
4. **NOT TESTED: `event_relations` at scale.** 5 000 relations; the expression
   index result is trustworthy, the absolute numbers are not extrapolated.
5. **NOT TESTED: redaction of secrets that are not in a `KEY=value` or a known
   flag position** — see §7.5. This is a design limit, not a gap to be closed by
   more patterns.
6. **NOT TESTED: credibility levels for relations that span a daemon restart.**
   Restart reconciliation exists in the schema; the level assignment across a
   restart is unmeasured.
7. **NOT TESTED: non-UTF-8 payloads.** JSON text storage assumes UTF-8.
8. **The 64-bit FNV-1a digest is linkable.** Two uses of the same credential
   produce the same value. That is intentional and documented; it is not
   anonymity, and a 64-bit space is small enough that an offline attacker with
   a candidate list could confirm guesses. Do not treat the digest as protection
   for the *secret itself*.
9. **q4 vs q4b is index-quality, not query cost.** With the index the query is
   16 µs; without it, 2.75 ms. Both numbers are true and neither generalises to
   a different query shape.

## 7. Conclusion

**PASS.** All seven hypotheses hold, including the two that are really about
*refusing* to do something: the fingerprint keeps an undecidable class separate
instead of pretending to resolve it, and the identity policy drops to `Unknown`
on collision instead of guessing.

What Phase 1 must adopt:

1. `event_id` as the only merge key; semantic fingerprints become
   `event_relations` rows with a level.
2. `sequence` as the ordering key; `ts_wall` for display only.
3. Collision-aware process identity with an explicit `Unknown` level.
4. The three permission-only relation kinds; no `caused_by` variant exists.
5. Many-to-many `event_operations`, never a single FK column.
6. Redaction before insert, with the digest documented as linkable.
7. An expression index on any relation query keyed by a JSON path, from
   migration 0001, not added later.

## 8. Gate

**May Phase 1 implement the correlation layer as specified above?

YES, with three conditions:**

1. **Two-tier dedup in the schema from the start.** `events` is keyed by
   `event_id` and has no semantic uniqueness constraint of any kind; the
   fingerprint lives only in derived tables. A Phase 1 that puts a unique index
   on a fingerprint will be unable to un-merge a wrong decision.
2. **`relation.kind` is a closed enum in the type system, not a string column
   with a comment.** The only enforcement that survives contact with a future
   contributor is one where `caused_by` cannot be spelled.
3. **Every relation carries a level and the level is monotone.** If the
   implementation cannot prove monotonicity, it must emit `Unknown` rather than
   a level it cannot defend.

**Explicitly deferred to a later phase:** real-traffic validation of the trap
classes, scale beyond 100 k rows, restart-spanning credibility, and any attempt
at positional-secret detection.
