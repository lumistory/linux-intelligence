# 0-H — Event deduplication, correlation and Operation Context

Authoritative run: **`results/20261123T142301Z-*`** (24 files), produced by
`./run.sh` at exit 0, 192 s wall. Host: Linux 7.2.8-zen1-2-zen, x86_64, 20
cores, `CLK_TCK=100`, rustc 1.99.0, SQLite 3.53.4 (system library, deliberately
not the bundled copy), `ulimit -n` 1024.

**Reproducibility.** The whole suite was run twice back to back
(`20261123T142301Z` and `20261123T143036Z`); both exited 0. The logic
experiments are byte-identical between runs (fixed seeds, fixed timestamps), and
the SQLite timings agree within a few percent — q1 p50 76.3 vs 75.3 us, q2 144.2
vs 142.4 us, q3 32.7 vs 32.9 us, insert 516 vs 507 ms. No cell moved by more than
8 %, so the numbers in §7 are stable enough to plan against and still obviously
measurements rather than constants.

Synthetic material is labelled `SYNTHETIC` where it is used. Kernel and database
facts (tick granularity, WAL behaviour, `kill -9`, migration, VACUUM) are
measured on real files and real processes.

## 1. Objective

Decide whether the Phase 0 event envelope plus a correlation layer can answer the
only question a user asks — *"I ran this command; what did it change, and can you
prove it?"* — without lying. Concretely: can duplicates be removed without
removing real work, can an out-of-order stream be reordered, can a file change be
attributed when the process identity key is known to collide, can the result be
expressed at a credibility level a machine can check, can one event belong to
zero, one or many operations, and does SQLite carry all of it through 100 000
rows, a `kill -9`, a schema migration and a retention sweep.

Seven hypotheses (H1–H7) were fixed in `README.md` before any measurement.

## 2. Hypothesis

| # | Hypothesis | Verdict |
|---|---|---|
| H1 | `event_id` dedup is necessary but insufficient: >= 10 % of duplicates arrive with a fresh id | **PASS** — measured 50.0 % |
| H2 | sequence reordering is strictly better than timestamp sorting inside a session; timestamps are the only option across sessions and they invert under quantisation | **PASS-WITH-LIMITS** |
| H3 | `pid`-only attribution produces wrong merges, and a detected `(pid, starttime)` collision must degrade to `Unknown` rather than guess | **PASS** — the alias is SYNTHETIC, its precondition is real |
| H4 | the five levels are a total function over a fixed evidence record, with no heuristic outside the table | **PASS** — 24/24 scenarios, 0 monotonicity violations |
| H5 | time adjacency is not causation and the API must be unable to say `caused_by` | **PASS** |
| H6 | one event belongs to 0..n operations; a single `operation_id` column cannot express that | **PASS** |
| H7 | SQLite (WAL, FK on, explicit `busy_timeout`) carries 100k rows with correlation queries well under 10 ms and survives `kill -9` | **PASS**, with one near-miss reported in §7.4 |

## 3. Method

- **One library, two binaries.** `corr0h` is the library; `hexp` runs the seven
  logic experiments, `hdb` the six SQLite experiments. 47 unit tests pin every
  claim this report makes about *code*; the binaries print the tables quoted
  below, and `results/` holds their verbatim output.
- **Ground truth by construction.** `dedup_exp` builds a 1050-event population
  where each event carries a truth label and a trap class, so precision and
  recall are counted, not argued. Origin command lines carry a unique serial
  argument so "distinct" is physically distinguishable from "duplicate" —
  otherwise the precision number would have been measuring the generator.
- **Real clock, real forks.** `run.sh` measures `starttime` granularity by
  forking 40 concurrent processes and reading field 22 of `/proc/<pid>/stat`,
  splitting on the last `)` so a `comm` containing `)` cannot shift the fields.
- **Real crashes, real files.** `SIGKILL` against a writer committing one
  transaction per event; `PRAGMA integrity_check` and `foreign_key_check`
  afterwards; a v1 database populated with 5000 rows before the v1 -> v2
  migration.
- Every step is bounded by `timeout`. No step deletes a previous result file:
  each run allocates its own `/tmp/li-phase0-h-<stamp>/` working directory.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` run as
  build steps, so a warning fails the run.

## 4. Raw results

Paths relative to `experiments/00-h-correlation/results/`, stamp
`20261123T142301Z`.

| file | content |
|---|---|
| `-run.log` | environment, build, fmt/clippy/test, per-step summary |
| `-dedup.log` | §4.1: baseline, 42-config sweep, trap classes, 3 seeds |
| `-reorder.log` | §4.2: 2 timestamp regimes x 6 depths, grace sweep, mid-stream attach |
| `-pidns-anchor.log` | §4.3 REAL tick-granularity measurement |
| `-pidreuse.log` | §4.3 SYNTHETIC alias, three attribution policies |
| `-confidence.log` | §4.4 rule table, 24 scenarios, distribution, monotonicity |
| `-causality.log` | §4.5 counterexamples, kind parsing, validation, rendering |
| `-context.log`, `-context-sql.log` | §4.6 0..n membership, in memory and in SQL |
| `-redact.log` | §7.6 two generations, 53 clean + 24 secret commands |
| `-mig-{v1,populate-v1,v1-verify,v2,after}.log` | §7.1 migration |
| `-bench-100k.log` | §7.4 100k insert, 4 query shapes, 1 write |
| `-wal-concurrency.log` | §7.2 1 writer/4 readers and 2 writers, two `busy_timeout` values |
| `-kill9.log` | §7.3 SIGKILL, integrity, torn-transaction check |
| `-retention-{time,size}-idx-{no,yes}.log` | §7.5 retention x FK indexing |

Provenance note: `results/hexp-full-20261123T141333Z.log` is a **stale artifact**
from an intermediate binary. It predates three fixes (the `LateArrival` outcome,
the watermark bug that made a reorderer stall, and the corrected R5 predicate)
and its tables therefore disagree with this report — for example it shows a max
buffer of 65 where the current code shows 30 at depth 32. It is left in place
rather than deleted (this experiment does not delete files) and **nothing in this
report is derived from it**.

### 4.1 Deduplication (SYNTHETIC)

Population: 1050 events = 500 origins + 300 duplicates (150 same-`event_id`
resends, 150 fresh-id semantic duplicates) + 250 near-miss negatives in five
classes of 50.

| scheme | key | TP | FP (decidable) | FN | precision | recall | false-merge |
|---|---|---|---|---|---|---|---|
| A: exact `event_id` | `event_id` | 150 | 0 | 150 | 1.0000 | **0.5000** | 0.0000 |
| B: fingerprint, 200 ms triple, session | `type_class / session / subject / bucket±1` | 300 | 50 | 0 | 0.8571 | 1.0000 | 0.1429 |
| B: fingerprint, 200 ms triple, session+process | + `pid:starttime` | 300 | 0 | 0 | **1.0000** | 1.0000 | 0.0000 |

Exact-id dedup is free and has **zero** false positives, and it misses exactly
half of all duplicates: the 150 semantic ones. That single number is H1.

**Sweep: 42 configurations** (7 windows x 2 bucket modes x 3 key variants), all
run in full. The shape is stable across every one of them:

| window | bucket | key | precision | recall | note |
|---|---|---|---|---|---|
| 10 ms | single | +session+process | 1.0000 | 0.5700 | too narrow: 129 duplicates missed |
| 50 ms | triple | +session | 0.8571 | 1.0000 | |
| 250 ms | triple | +session | 0.8571 | 1.0000 | |
| 500 ms | triple | +session | 0.8571 | 1.0000 | |
| 2000 ms | triple | +session | 0.8571 | 1.0000 | |
| 250 ms | single | no session | 0.7448 | 0.9633 | session dropped: every D2 merges |

Bucket mode matters more than width. `single` loses recall because a duplicate
straddling a bucket boundary hashes to the other bucket; `triple` (hash the
bucket and its two neighbours) fixes that at the cost of a wider confusion
window. Past 250 ms the numbers stop moving, so width is not the dial — the
**key** is.

**Which key component earns its place** (200 ms, triple):

| trap class | n | no session | +session | +session+process |
|---|---|---|---|---|
| D1 same cmd+session+window (UNDECIDABLE) | 50 | 50 | 50 | 50 |
| D2 same cmd, other session, same window | 50 | 50 | **0** | 0 |
| D3 same cmd, far in time | 50 | 0 | 0 | 0 |
| D4 other cmd, same window | 50 | 0 | 0 | 0 |
| D5 same cmd+session+window, different `pid:starttime` | 50 | 50 | 50 | **0** |
| origin (decidable negative) | 500 | 0 | 0 | 0 |

D1 is the honest residue: same command text, same session, inside the window, and
nothing in the content separates a human double-tap from a double-report.
Counting it as a false positive would score the scheme against an impossible
question, so it gets its own column and is never hidden.

**Stability across three seeds:** exact recall 0.5000 every time; fingerprint
precision (decidable) 0.8571, precision (all negatives counted) 0.7500, recall
1.0000.

**Measured blind spot.** `make -j8` from a hook and `/usr/bin/make -j8` from
`/proc/<pid>/cmdline` both come back `Fresh`: the subject is normalized command
text, and an absolute path does not hash equal to the typed command. A real
miss, reported as one, not hidden behind a tuning knob.

### 4.2 Out-of-order streams (SYNTHETIC)

500 events, 1 ms apart. `received` = order of arrival; `ts-sort` = arrival
sorted by `ts_mono_ns`; `sequence` = arrival through the reorderer. Metric is
order agreement over all pairs (1.0 = perfect, 0.5 = random).

| depth | received | ts-sort (distinct ts) | seq (distinct) | ts-sort (10 ms quantised) | seq (quantised) |
|---|---|---|---|---|---|
| 1 | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.0000 |
| 2 | 0.9990 | 1.0000 | 1.0000 | 0.9990 | 1.0000 |
| 4 | 0.9970 | 1.0000 | 1.0000 | 0.9974 | 1.0000 |
| 8 | 0.9931 | 1.0000 | 1.0000 | 0.9952 | 1.0000 |
| 16 | 0.9846 | 1.0000 | 1.0000 | 0.9927 | 1.0000 |
| 32 | 0.9710 | 1.0000 | 1.0000 | **0.9920** | 1.0000 |

`sequence` is 1.0000 in every cell: no ties by construction (0-C measured the gap
as exact). Timestamp sorting looks perfect with distinct timestamps and
**degrades under 10 ms quantisation** — 0.9920 at depth 32, i.e. 0.8 % of pairs
ordered wrong by a sort that had no information to do better. Max buffer equals
the disorder depth (<= 30), so a 64-slot buffer covers depth 32 here.

**Grace-window sizing.** Measured reorder delay (the true predecessor's arrival
lag) at depth 8: n=220, p50 300 us, p90 600 us, **p99 700 us**, max 700 us.

| grace | forced gaps | late arrivals | max buffered | agreement |
|---|---|---|---|---|
| 1–100 us | 111 | 322 | 1 | 0.9931 |
| 250 us | 121 | 264 | 2 | 0.9937 |
| **1000 us** | **0** | **0** | 9 | **1.0000** |

Below the reorder delay the reorderer does not lose events: it converts a
reordering into a `LateArrival`, emitted immediately and flagged. That is 0-loss
and 0-stall, which is the property a consumer needs; it is not "in order", and
saying so would be the lie.

**Recommendation: `grace_ns >= 4 x p99 reorder delay`, rounded up to the
transport's own jitter.** Measured here p99 = 0.7 ms, so grace = 1 ms, which
produced 0 forced gaps and 1.0000 agreement. Taking the value from a constant
("500 ms") is indefensible: the number belongs to the measured arrival
distribution, and that measurement is cheap enough to redo per deployment.

**Mid-stream attach.** A consumer starting at sequence 100 of 200 with
`initial_sequence: None` emitted 100, held 0, forced 0 gaps. With a stored
checkpoint (`Some(n)`), anything older becomes a `LateArrival` — emitted and
flagged, never silently dropped, never stalling. Both paths are 0-loss; the
default must be explicit, because a reorderer that keys grace on "more events
arriving" never flushes on a quiet session.

### 4.3 Process identity (REAL anchor + SYNTHETIC alias)

**Real.** 40 processes forked in one loop: 40 distinct pids, **2 distinct
`starttime` values** (36 at tick 1294535, 4 at tick 1294534). A tick is 10 ms and
fork is far faster, so "same `starttime`" is the normal case, not an edge case.

What this does **not** prove: a `(pid, starttime)` *alias* additionally needs the
pid to be recycled inside one tick, which requires wrapping `kernel.pid_max` —
privilege this experiment does not have. That step is `NOT TESTED` on the kernel
and is tested SYNTHETICALLY below; its precondition is measured above.

**SYNTHETIC.** Two processes share pid 4242 and starttime 900. A (`editor`)
writes `a.txt` and exits; B (`make`) starts inside the same tick and writes
`b.txt`.

| policy | writer of `a.txt` | writer of `b.txt` | outcome |
|---|---|---|---|
| pid only | pid 4242 | pid 4242 | WRONG: both writes to one actor |
| `(pid, starttime)`, blind | 4242:900 | 4242:900 | WRONG: same alias, still merged |
| `(pid, starttime)` + collision detection | ambiguous | ambiguous | `Unknown`, no claim |

The registry detected the collision (`ExitedThenReappeared`), recorded both
incarnations `[1, 2]`, and poisoned the key. User-visible effect:

| question | pid only | collision-aware | ground truth |
|---|---|---|---|
| files changed by pid 4242 | `a.txt, b.txt` | `Unknown` | `a.txt` by A, `b.txt` by B |
| did `make b` modify anything? | `a.txt` (invented) | `Unknown` | `b.txt` |
| did `editor a.txt` modify anything? | `a.txt, b.txt` (invented) | `Unknown` | `a.txt` |

Two of three answers are wrong under pid-only, and wrong in the dangerous
direction: they assert an actor that cannot have done it.

`classify(perfect evidence, collided key)` returns **Unknown / R0**, because R0
is evaluated first — no amount of extra evidence outvotes a broken identity.
`classify(pid equality only)` returns **Weak / R6**: a hook reporting no
`starttime` can never reach `Probable`, because a pid is not an identity.

**Recovery path.** A collision is permanent only when the incarnations are
indistinguishable. They are distinguishable when any of `comm`, the `cmdline`
hash, or an observed `ProcessExit` differs — all three are detected above. When
none differ, `ambiguous = 1` is persisted (`processes.ambiguous` in
`migrations/0001_init.sql`), so the degradation survives a restart instead of
being recomputed differently.

### 4.4 Five credibility levels

Rule table, evaluated in order, first match wins:

| rule | level | predicate (all conjuncts required) |
|---|---|---|
| R0 | Unknown | actor identity `Ambiguous` — `(pid, starttime)` collided |
| R1 | Unknown | no process identity, and the only link is a shared `session_id` |
| R2 | Unknown | a file event exists and lies outside the actor's interval |
| R3 | Confirmed | keyed actor AND (self-observed OR fork edge) AND inside interval AND path tied to the command |
| R4 | Strong | >= 2 independent source classes AND >= 1 kernel-observed |
| R5 | Probable | keyed actor AND (self-observed OR fork edge) AND inside interval AND **no file evidence tied to the command** |
| R6 | Weak | inside the interval, nothing more |
| R7 | Unknown | residual |

**Independence, defined rather than felt.** Two claims are independent iff (1)
their `SourceClass` differs, (2) neither is marked derived from the other, and
(3) the pair is one of `declared/proc`, `declared/fs`, `proc/fs`. Two bash hooks
firing twice are **one** source; a kernel fs claim flagged `independent: false`
does not corroborate the proc claim it was derived from.

**24 scenarios, 24 PASS**, each asserting level *and* rule:

| # | scenario | expected | actual | verdict |
|---|---|---|---|---|
| S01 | actor is the reported writer, path tied, inside interval | Confirmed / R3 | Confirmed / R3 | PASS |
| S02 | one-hop fork chain, path tied | Confirmed / R3 | Confirmed / R3 | PASS |
| S03 | four-hop fork chain (pipeline), path tied | Confirmed / R3 | Confirmed / R3 | PASS |
| S04 | collision, everything else perfect | Unknown / R0 | Unknown / R0 | PASS |
| S05 | collision + two independent corroborations | Unknown / R0 | Unknown / R0 | PASS |
| S06 | only a shared `session_id` | Unknown / R1 | Unknown / R1 | PASS |
| S07 | no actor at all, file event present | Unknown / R1 | Unknown / R1 | PASS |
| S08 | file event outside the actor's interval | Unknown / R2 | Unknown / R2 | PASS |
| S09 | declared + kernel fs agree, path tied, no fork edge | Strong / R4 | Strong / R4 | PASS |
| S10 | declared + kernel proc agree, no file evidence | Strong / R4 | Strong / R4 | PASS |
| S11 | kernel proc + kernel fs agree | Strong / R4 | Strong / R4 | PASS |
| S12 | two declared shells, hook fired twice | Weak / R6 | Weak / R6 | PASS |
| S13 | fs claim marked derived from proc | Weak / R6 | Weak / R6 | PASS |
| S14 | fork chain, inside interval, no fs evidence | Probable / R5 | Probable / R5 | PASS |
| S15 | fork chain, untied path observed | Probable / R5 | Probable / R5 | PASS |
| S16 | actor is the writer but path untied | Probable / R5 | Probable / R5 | PASS |
| S17 | pid-only actor, tied path, inside interval | Weak / R6 | Weak / R6 | PASS |
| S18 | pid-only, no window overlap | Unknown / R7 | Unknown / R7 | PASS |
| S19 | keyed actor, no lineage, untied path | Weak / R6 | Weak / R6 | PASS |
| S20 | keyed actor, no file event, no lineage | Weak / R6 | Weak / R6 | PASS |
| S21 | three independent classes, no fork edge | Strong / R4 | Strong / R4 | PASS |
| S22 | inferred-only corroboration, fork chain perfect | Confirmed / R3 | Confirmed / R3 | PASS |
| S23 | keyed actor, tied path, containment unknown | Unknown / R2 | Unknown / R2 | PASS |
| S24 | one kernel source corroborating only itself | Weak / R6 | Weak / R6 | PASS |

Distribution: Confirmed 4, Strong 4, Probable 3, Weak 6, Unknown 7.
Monotonicity (add one corroboration to each of the 24; level must not drop):
**0 violations**.

Three scenarios failed on the first rule set and exposed two real bugs, both
fixed rather than re-specified: R1 demoted a *keyed* actor carrying a
contradictory `session_only` flag, and R5 required `file_side == None` so an
*untied* path fell through to Weak. R5's predicate is now
`file_side != PathTiedToCommand`, the correct reading of "no file-side evidence
*for this command*".

### 4.5 Causality constraint

| counterexample | naive claim | why it is false | defensible relation | level |
|---|---|---|---|---|
| background task outlives its command | "make modified build/out.o" | the hook fires at command exit; children keep writing for seconds after | `spawned_by` | Confirmed |
| delayed writeback | "echo modified notes.txt" | `echo x > f` returns after `write(2)`; writeback lands later, so the file event can postdate the exit | `modified_during` | Weak |
| editor rename-save | "vim modified init.lua" | vim writes a temp file and `rename(2)`s it over the target, so the modified path is the temp path | `observed_within` | Weak |
| two writers, one file | "cmd-a modified shared.log" | inotify carries no pid (0-E), so a file two processes appended to cannot be attributed | `observed_within` | Unknown |

Kind parsing: `observed_within` / `spawned_by` / `modified_during` accepted;
`caused_by`, `causes`, `led_to`, `result_of` **rejected**. Enforced twice — the
parser returns `None` and the schema `CHECK (kind IN (...))` refuses the row; the
unit test `caused_by_is_rejected_by_the_schema` asserts the `INSERT` fails.

Validation: `spawned_by` without a fork edge -> `REJECTED NoForkEvidence`;
`Confirmed` from inferred inputs only -> `REJECTED InferredCannotConfirm`.

Rendering is capped by **kind**, not by level. At `Confirmed`, `modified_during`
renders "was modified during", `observed_within` renders "was observed within",
and only `spawned_by` renders "was spawned by" — the one relation with a
kernel-observed direction.

### 4.6 Operation Context 0..n

Rust store and SQL schema, both measured:

| query | result |
|---|---|
| operations of `ev-1` | `op-edit` |
| operations of `ev-2` | `op-edit`, `op-recover` |
| events of `op-edit` | `ev-1`, `ev-2` |
| events of `op-recover` | `ev-2` |
| events belonging to no operation | `ev-3` |
| operations with no event | `op-untouched` |

Same answers in SQL against the real schema: operations of `ev-2` = 2, events of
`op-recover` = 1, orphan events = 1, empty operations = 1, and `ev-2`'s two
memberships carry **different levels** (`op-edit` Probable/R5, `op-recover`
Strong/R4). That is why the level lives on the edge and not on the event.

Retraction: after detaching `ev-2` from `op-recover`, live memberships of `ev-2`
= 1 while 2 membership rows are retained. The history of a link survives its own
retraction.

## 5. Analysis

**Deduplication must be two-level, and the levels have opposite jobs.** Exact
`event_id` is the authority for *storage*: zero false positives, free, and so it
must never be relaxed. Its recall is 0.5000 on a realistic duplicate population,
so it cannot be the authority for *facts*. The semantic fingerprint recovers
everything exact-id misses, and its cost is decided entirely by the key: dropping
the session merges 100 % of the "same command in another terminal" class, and
dropping the process key merges 100 % of the "same script, second concurrent run"
class. Both are decidable negatives, which is why the recommendation is
`type_class | session | subject@pid:starttime | bucket(±1)` at 200–500 ms — and
why a fingerprint match may only *attach corroboration*, never delete a row. The
one class it cannot resolve (`D1`) is reported as such.

**Ordering is a per-session problem with a per-transport parameter.** `sequence`
is perfect and timestamps are not, but only inside a session: 450 of 499 adjacent
pairs share a 10 ms tick at 1 ms spacing, so `ORDER BY ts_mono_ns` is deciding
ties by its own implementation detail. Across sessions there is no `sequence`
space at all, so the correlator must fall back to `ts_mono_ns` **and store the
±1 tick uncertainty** rather than pretend the order is known. The grace window is
a measured quantity (4 x p99 reorder delay = 1 ms here), and the flush must be
clock-driven rather than arrival-driven — otherwise a lost event in a quiet
session sits in the buffer forever, which is exactly when it matters.

**Process identity is the load-bearing failure.** `(pid, starttime)` is not a
key, and the measurement says why: 40 concurrent forks produce 2 distinct
`starttime` values. The registry's answer — detect, poison, degrade to `Unknown`,
persist the poison — is the only version that does not lie, and the classifier
enforces it by checking the collision **first**, before any evidence that might
outvote it.

**The levels are mostly about refusing.** Of 24 scenarios, 7 land on `Unknown`
and 6 on `Weak`: the majority of hand-built cases are cases where the system
declines to answer. That is the intended shape. `Confirmed` needs four conjuncts
including a *tied* path, a high bar precisely because 0-E established that
inotify cannot supply the actor.

**The schema's real cost driver is foreign keys, not row count.** Deleting 10 000
of 100 000 events took **75 s** without indexes on the FK child columns and
**65 ms** with them — a 1150x difference caused by `duplicate_of`'s
self-referencing `ON DELETE SET NULL`, which scans `events` once per deleted row.
Nothing about 100 000 rows is hard; a single missing index made retention look
like a design failure.

**Redaction is a lossy filter with a positional blind spot.** V2 catches 24/24
secret-bearing commands with 0/53 false positives on a deliberately adversarial
clean corpus, but `./deploy s3cr3tvalue` passes through untouched, because a
positional value is indistinguishable from a filename. No regex generation fixes
that; the mitigation is to not capture it, or to let the user mark the position.

## 6. Known limitations

1. **The `(pid, starttime)` alias is SYNTHETIC.** Its precondition (coarse tick,
   many processes per tick) is measured on the kernel; forcing an actual alias
   needs pid recycling inside one tick, which needs `kernel.pid_max`
   manipulation and therefore privilege this experiment does not have.
   `NOT TESTED` on the kernel.
2. **All correlation input is synthetic.** No run feeds the correlator from real
   `bash` hooks, a real procfs poller and a real inotify stream at the same time.
   0-A/0-B/0-E measured those transports individually; this experiment measures
   what the correlator does with them, not whether the transports produce what
   the generator produces. The generator's cross-source skew is a modelled
   0–30 ms, not an observed distribution.
3. **`D1` is undecidable, not solved.** 50/50 double-taps merge under every
   configuration tested. Whether real duplicate reports and real human
   double-taps are separable at all is `NOT TESTED`.
4. **One stream shape per ordering test.** Single session, 1 ms spacing, 500
   events. A 10 000-event burst, a stalled transport or interleaved sessions
   would exercise the buffer bound differently.
5. **Two writers is the tested write-contention ceiling.** 1 writer + 4 readers
   and 2 writers were measured; 8 writers, or writers in separate databases on
   one filesystem, are `NOT TESTED`.
6. **`synchronous=NORMAL`, not `FULL`.** The `kill -9` result is for `NORMAL`.
   `FULL` would add an fsync per commit and change both the throughput number and
   the crash boundary. Power loss (not just a process kill) is `NOT TESTED`.
7. **Retention was measured at 10 % deletion, not steady-state churn.** The
   75 s / 65 ms comparison is the finding; whether a continuous trim keeps up
   under a real ingest rate is `NOT TESTED`.
8. **The redaction corpora are 53 + 24 hand-written commands.** They were written
   to contain plausible false positives (ports, digits-only values, variable
   references, the word "password" in prose), but they are not a sample of real
   shell history — that would have meant reading the host's history files, which
   is out of bounds. A real-history false-positive rate is `NOT TESTED`.
9. **`recovery_plans` and `approvals` are schema-only.** No planner wrote a row;
   their constraints are exercised by the migration, not by behaviour.
10. **No network fetch was attempted.** Every crate came from the local cache and
    the build ran `--offline` throughout. `rusqlite` links the system SQLite
    3.53.4 rather than a bundled copy, so the numbers describe the library that
    would actually run here.

## 7. Conclusion

### 7.1 Migration v1 -> v2 (real file, 5000 rows pre-loaded)

| check | result |
|---|---|
| v1 shape | 16 columns in `events`, 11 in `sessions`, populated with 5000 events + 2 sessions |
| upgrade | `schema_version: 1 -> 2, applied=[2]` |
| data after upgrade | 5000 events, 2 sessions — no loss |
| new columns on old rows | `command_text IS NULL`, `redaction_rules IS NULL` for every sampled row |
| integrity / FK | `integrity_check=ok`, `foreign_key_violations=0` |
| ledger | v1 checksum `546433d699109e0d`, v2 checksum `c03348a47a47c7a6` |
| re-running `migrate` | applies nothing, verifies checksums, returns `[]` |

Additive-only (`ALTER TABLE ADD COLUMN` plus one expression index) is what made
this safe; a table rebuild would have needed a full copy inside a riskier
transaction.

### 7.2 WAL concurrency

| configuration | writer | 4 readers | reader p50 | reader p99 |
|---|---|---|---|---|
| 1 writer + 4 readers, `busy_timeout=5000` | 208 294 commits, 0 errors | 0 errors, 0 busy | 132–142 us | 367–398 us |
| 1 writer + 4 readers, `busy_timeout=0` | 330 714 commits, 0 errors | 0 errors, 0 busy | 290–305 us | 1243–1826 us |
| 2 writers, `busy_timeout=5000` | 30 093 + 320 827, 0 errors | — | — | — |
| 2 writers, `busy_timeout=0` | A 316 922, **B failed: `database is locked`** | — | — | — |

Readers never block under WAL, so `busy_timeout` is **not** justified by reader
contention. It is justified by a second writer, where 0 ms turns a normal overlap
into a hard failure and 5000 ms turns it into serialisation with zero errors.
Recorded value: **`busy_timeout = 5000 ms`**, `synchronous=NORMAL`,
`wal_autocheckpoint=1000`, `cache_size=-65536`, `foreign_keys=ON` per connection.

The `busy_timeout=0` reader row is not a reader bug: with no backoff the writer
runs 1.6x faster and starves the readers of page-cache locality, so per-query
latency rises. Reported as measured, not smoothed.

### 7.3 `kill -9` against a writer

Writer commits one transaction per event; `SIGKILL` after 1.5 s of steady commits.

| observation | value |
|---|---|
| sidecars at the crash | `kill9.db` 19 312 640 B, `-wal` 4 185 952 B, `-shm` 32 768 B |
| after open: `PRAGMA integrity_check` | **`ok`** (142 ms) |
| `foreign_key_check` | 0 violations |
| visible rows | 75 784, `max_sequence = 75 783` |
| torn transaction | **none** — `max_sequence + 1 == row count` |
| sidecars after the clean open | `-wal` and `-shm` gone (checkpointed) |
| second writer after the crash | 45 105 commits, `integrity_check=ok` (225 ms) |

The uncommitted tail in the 4 MB `-wal` was discarded by recovery, and the visible
rows form a contiguous committed prefix — no half-applied transaction.

### 7.4 100 000 rows: insert and the query shapes

| operation | p50 | p95 | p99 | max |
|---|---|---|---|---|
| insert 100 000 (one txn, 5.17 us/row, 193 432 rows/s) | total 516 ms | — | — | — |
| q1 by session (200 rows) | **76.3 us** | 98.3 us | 168.1 us | 279.1 us |
| q2 by time range (1 s slice, ~1000 rows) | **144.2 us** | 206.8 us | 266.7 us | 321.1 us |
| q3 by operation (50 rows) | **32.7 us** | 38.3 us | 45.4 us | 100.7 us |
| w1 single-row upsert | 8.3 us | 9.4 us | 13.3 us | 78.8 us |

Database file 32.2 MB (7852 pages) for 100k events + 200 operations + 10k
memberships + 5000 relations. All three required shapes are two to three orders
of magnitude under the 10 ms bar.

**The near-miss.** A fourth, self-imposed query — "relations touching this path",
joining `event_relations` to `json_extract(events.payload,'$.path')` — took
**2664 us p50 / 3041 us p99** without an index, because it scanned 5000 relations
x 100k events through a JSON extraction. Adding
`CREATE INDEX ... ON events(json_extract(payload,'$.path'))` (44 ms to build)
brought it to **16.6 us p50 / 24.4 us p99**, a 160x improvement. It stays under
10 ms either way, but the unindexed form is the shape that degrades
non-linearly, so the index belongs in the schema recommendation rather than in a
footnote.

### 7.5 Retention

10 000 of 100 000 events deleted (10 %), batch 5000:

| mode | FK child indexes | trim | file before -> after trim | VACUUM | file after VACUUM |
|---|---|---|---|---|---|
| by time | **no** | **74 595 ms** | 32.2 MB -> 32.2 MB | 55 ms | 27.8 MB |
| by time | yes | **64 ms** | 33.2 MB -> 33.2 MB | 62 ms | 28.6 MB |
| by size | **no** | **75 604 ms** | 32.2 MB -> 32.2 MB | 62 ms | 27.8 MB |
| by size | yes | **65 ms** | 33.2 MB -> 33.2 MB | 60 ms | 28.6 MB |

Both modes stay inside 3 minutes even in the bad case, so the 100k scale was not
shrunk — but the bad case is 75 s for a 10 % trim, which extrapolates to roughly
12 minutes for a full history. The cause is an unindexed self-referencing FK
(`duplicate_of ... ON DELETE SET NULL`), not row count. `VACUUM` reclaims 4.4 MB
in ~60 ms and every run ends `integrity_check=ok`.
`PRAGMA auto_vacuum = 0`, so `incremental_vacuum` is a no-op (0 ms): incremental
reclaim requires `auto_vacuum=INCREMENTAL` set **before** the first table exists,
i.e. it is a schema-creation decision, not a maintenance one.

### 7.6 Redaction

| generation | clean n | false positives | FP rate | secret n | missed | miss rate |
|---|---|---|---|---|---|---|
| V1 (flags, env names, auth header) | 53 | 0 | 0.0000 | 24 | 6 | 0.2500 |
| V2 (+ URL userinfo, `key: value`) | 53 | **0** | **0.0000** | 24 | **0** | **0.0000** |

The three required cases, verbatim from the run:

| in | out | rule |
|---|---|---|
| `mysql -u root -pS3cr3tPw -h db` | `mysql -u root -p[REDACTED] -h db` | `flag-short-p-glued` |
| `export API_KEY=abcdef123456` | `export API_KEY=[REDACTED]` | `env-assign` |
| `curl -H "Authorization: Bearer <jwt>" https://x` | `curl -H "Authorization: Bearer [REDACTED]" https://x` | `auth-header` |

Per-rule coverage on the secret corpus: `env-assign` 7, `url-userinfo` 4,
`auth-header` 3, `flag-value-assign` 3, `flag-value-separate` 3,
`flag-short-p-glued` 2, `key-colon-value` 2. V1's 6 misses are exactly the two
rules V2 adds (3 URL-userinfo cases, 3 `key: value` cases).

Zero false positives on 53 adversarial clean commands is the number that matters,
and it required a specific defence: **digits-only values are never treated as
secrets**. `mysql -p3306`, `ssh -p 2222`, `export PORT=8080` and
`export TIMEOUT=30` all survive because of it. `export DATABASE_URL=...` also
survives, because `*_URL` was deliberately removed from the secret-env list after
the first run flagged `postgres://localhost/app` — a credential in a URL lives in
its *userinfo* section, so masking the whole value is the wrong fix and the
`url-userinfo` rule is the right one.

Residual, and structural: `./deploy s3cr3tvalue` is **not** masked by either
generation. A positional argument cannot be distinguished from a filename.

### Overall

**PASS-WITH-LIMITS.**

Six of seven hypotheses are unqualified PASS. H2 is PASS-WITH-LIMITS for one
reason: reordering is only definable inside a session, and across sources the
correlator has nothing but a 10 ms-quantised timestamp, so the report recommends
storing the uncertainty rather than claiming an order. H3 is a PASS whose *alias*
half is synthetic. Nothing measured contradicts a Phase 1 implementation; three
things (real cross-source correlation, a kernel-forced alias, `synchronous=FULL`
crash behaviour) are `NOT TESTED` and must not be cited as verified.

## 8. Gate

### 8.1 May Phase 1 implement the correlation algorithm? — **YES, with conditions**

1. Deduplication **must** be two-level: exact `event_id` decides storage, the
   semantic fingerprint only *attaches corroboration*. A fingerprint match may
   never delete or overwrite a row — the false-merge rate is non-zero by
   construction (`D1`), and a deletion cannot be undone.
2. The fingerprint key **must** include `session_id` and, when `starttime` is
   available, `pid:starttime`. Dropping either costs 50 of 50 decidable
   negatives in its class (§4.1: `D2`, `D5`).
3. Reordering **must** flush on a clock, not on arrival. Grace **must** be
   derived from a measured p99 reorder delay (4 x p99, 1 ms here), and
   `LateArrival` must be a first-class outcome, not a drop.
4. Cross-source ordering **must** carry an explicit ±1 tick (10 ms) uncertainty
   in the stored record. Ordering across sources is not knowable better than
   that.
5. `relation.kind` is limited to `observed_within` / `spawned_by` /
   `modified_during` in the parser **and** in the schema `CHECK`. `caused_by` is
   unrepresentable at both layers.
6. Event -> operation membership **must** be a join table with per-edge `level`,
   `attached_by` and `detached_at`. No `events.operation_id` column, ever.

### 8.2 Which level must stay `Unknown` in Phase 1 — **`Confirmed` for file attribution**

`Confirmed` requires `PathTiedToCommand`: the observed path must be tied to the
command's own text. That works for `vim notes.md` and fails for every command
that *computes* its paths — compilers, package managers, `git`, `make`, test
runners — which is the majority of the writes that matter. Phase 1 has no
privileged source to close that gap (0-E: inotify carries no pid, eBPF needs
`CAP_BPF`/`CAP_PERFMON`). Therefore:

- **File attribution in Phase 1 caps at `Probable`.** A would-be `Confirmed`
  file-modification relation must be emitted as `Unknown` with the reason
  `path_not_tied` — not silently downgraded, and never upgraded on a time match.
- `Confirmed` **is** available for `spawned_by`, because `ppid` at fork time is
  kernel-observed. It must be labelled a *containment* relation, not a causal
  one: a parent that spawns `sh -c 'rm -rf build'` computed nothing.
- `Strong` needs two independent classes and Phase 1 has exactly three
  (`declared`, `kernel_proc`, `kernel_fs`). It is reachable but rare, and the
  independence flag must be set per claim, never inferred from class alone
  (scenario S13).

### 8.3 May the SQLite schema be carried forward? — **YES, with three hard constraints**

1. **No `events.operation_id`.** Membership is
   `event_operations(event_id, operation_id)` with `level`, `attached_by`,
   `detached_mono_ns`, PK `(event_id, operation_id)`. Measured: one event in two
   operations at two different levels, one event in none, one operation with no
   events — none expressible in a single column (§4.6).
2. **`(pid, starttime)` must never be `PRIMARY KEY` or `UNIQUE`.** The key
   carries `incarnation_seq` and `ambiguous`, and `ambiguous = 1` must survive a
   restart. Relations through an ambiguous key are `Unknown` (R0), and R0 is
   evaluated before any corroboration.
3. **Every foreign-key child column must be indexed.** Measured 75 s vs 65 ms for
   a 10 % trim (§7.5) — a 1150x factor from one missing index on a
   self-referencing `ON DELETE SET NULL`. Retention without it is not a policy,
   it is an outage.

Also carried forward, as measured rather than asserted: WAL +
`synchronous=NORMAL` + `busy_timeout=5000` + `foreign_keys=ON` **per connection**
(SQLite's default is OFF, so a prototype that forgets it "passes" while orphaning
rows); additive-only migrations with a checksum ledger; the expression index
`events(json_extract(payload,'$.path'))` for path queries (160x); and the
`event_relations` `CHECK` on the three kinds together with `rule` and `level` on
every edge, so a credibility claim can always be re-derived from the row that
made it.

### 8.4 Conditions before Phase 1 relies on any of this

1. Re-run the dedup population with duplicates produced by *real* double
   instrumentation (a hook registered twice), not by a generator.
2. Measure cross-source timestamp skew between a real bash hook, a 200 ms procfs
   poller and inotify. The generator's 0–30 ms is a guess, and the fingerprint
   window depends on it.
3. Attempt a real `(pid, starttime)` alias under `unshare -Ur` with a small
   `kernel.pid_max` if one can be set without privilege; otherwise keep the
   capability record honest that the alias is modelled.
4. Decide `synchronous=FULL` vs `NORMAL` on measurement, not on this report: the
   `kill -9` result here is `NORMAL` only.

> **路径替换说明**：本次公开前对 `results/` 与脚本里的绝对路径做了字符串替换，换成两个占位符 `<WORKSPACE>` 与 `<HOME>`。
> 规则、未改动的量、以及原始形态见 [`../SCRUB-NOTICE.md`](../SCRUB-NOTICE.md)。
