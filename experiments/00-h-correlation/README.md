# 0-H — Event deduplication, correlation and Operation Context

Status: **in progress** (README written before any measurement, per
`docs/00-phase0-protocol.md` §0.2).

## 1. Objective

Phase 0 has so far produced isolated facts about isolated transports: shells
emit hook events (0-A), scripts have a lifecycle (0-B), IPC delivers events with
at-most-once semantics per `event_id` (0-C), procfs gives `(pid, starttime)`
identity (0-E). Nothing yet answers the only question a user actually asks:

> "I ran this command; what did it change, and can you prove it?"

This experiment builds the *correlation layer prototype*: given a stream of
envelope events from several sources (shell hooks, procfs poller, inotify), can
we (a) drop duplicates without dropping real work, (b) put an out-of-order
stream back in order, (c) attribute a file change to a process when the process
identity key is known to be collision-prone, (d) express the result with a
credibility level that a machine can check, (e) store it in SQLite so that one
event may belong to several operations, or to none, and (f) survive a `kill -9`,
a schema migration, 100k rows and a retention sweep.

## 2. Hypotheses (stated before measurement)

| # | Hypothesis | Falsified if |
|---|---|---|
| H1 | `event_id` exact dedup is necessary but **not sufficient**: a meaningful share (>= 10 %) of real duplicates arrive with a *fresh* `event_id`, so a semantic fingerprint is needed. | semantic duplicates are < 10 % of the duplicate population, or the fingerprint's false-positive rate exceeds the duplicate rate it removes |
| H2 | Sequence-based reordering is strictly better than timestamp sorting *within* a session, and timestamp sorting is the only option *across* sessions, where it inverts order under clock quantisation. | sequence reordering loses correctness on gaps, or timestamp ordering survives `CLK_TCK` quantisation |
| H3 | `pid`-only attribution produces demonstrably wrong merges between two processes that share `(pid, starttime)`, and `(pid, starttime)` collisions can be detected at *write* time and must degrade the result to `Unknown` rather than guess. | no wrong merge can be constructed, or collisions are always detectable without degradation |
| H4 | A five-level credibility scale (Confirmed / Strong / Probable / Weak / Unknown) can be implemented as a pure, total function over a fixed evidence record, with no heuristics outside the rule table. | any scenario needs a rule not expressible as a predicate over the evidence record |
| H5 | Time adjacency is not causation and the API must not be able to say `caused_by` when the evidence is only temporal. | a concrete counterexample cannot be constructed |
| H6 | One event can belong to 0..n operations and the schema must not force a single `operation_id` column. | a normalised (single-FK) schema passes all the multi-context queries |
| H7 | SQLite (WAL, FK on, `busy_timeout`) carries this workload at 100k rows with sub-10 ms correlation queries and survives `kill -9` without corruption. | `integrity_check` fails, or a reader errors during 1-writer/4-reader, or p99 query latency >= 10 ms |

## 3. Method

1. **Synthetic-first, with real-kernel anchors.** Correlation logic is exercised
   on synthetic streams because ground truth is otherwise unknowable; every
   synthetic claim is labelled `SYNTHETIC` in the report. The `(pid, starttime)`
   collision is *also* attempted against the live kernel (0-E's finding), and
   the SQLite behaviours (`kill -9`, WAL, VACUUM) are measured on real files.
2. **Ground-truth-labelled generator.** `synth.rs` emits events tagged with the
   fact they belong to, so precision/recall of every dedup scheme is measured,
   not argued.
3. **Rule engine, not prose.** The five credibility levels are a function
   `classify(&Evidence) -> Level` plus a 20+ row scenario table asserted by the
   harness; a mismatch fails the build.
4. **SQLite measured under stress**: 100 000 rows, 1 writer + 4 reader
   processes, `kill -9` mid-transaction, v1 -> v2 migration on a populated
   database, 10 000-row batched retention vs `VACUUM`.
5. **Redaction measured with a false-positive corpus**, two regex generations,
   so the "did we break normal commands" number is empirical.

Everything runs through `./run.sh` (`set -euo pipefail`), each step bounded by
`timeout`, raw output in `results/`.

## 4. Layout

```
README.md            this file (written first)
run.sh               driver, exits non-zero on failure
Cargo.toml
src/lib.rs           envelope, dedup, ordering, identity, confidence, relations, redact, db
src/bin/hexp.rs      logic experiments (dedup, reorder, pidreuse, confidence, causality, context, redact)
src/bin/hdb.rs       SQLite experiments (migrate, bench, wal-concurrency, kill9, retention)
migrations/0001_init.sql, 0002_add_event_redaction.sql
results/             raw logs, never hand-edited
REPORT.md            8-section report
```

## 5. Out of scope

No eBPF, no privileged attribution (0-E has established it is unavailable in a
sandboxed daemon). No real operation-recovery execution; `recovery_plans` and
`approvals` are schema-only. No cross-machine clock synchronisation.
