# ADR 0003 — Event storage and schema versioning

- Status: accepted (2026-11-27; supported by 0-H + Phase 1/4 implementation)
- Date: 2026-11-23

## Implementation notes (D5)

- Delivered in `li-journal`: WAL mode with `synchronous=NORMAL` default (D8), `BEFORE UPDATE/DELETE` append-only triggers on `events`, audited single-transaction `Journal::purge_before` emitting `EventsPurged`, forward migrations (`0001`..`0003`) with `idx_events_payload_path` and `idx_events_payload_new_path` (`P1-28`), and many-to-many `event_operations` / `event_relations`.

## Context

Raw facts must be append-only and never rewritten by inference. Operation
contexts and relations are derived and may be recomputed. SQLite is the local
store and must survive `kill -9` and be upgradeable.

## Decision (proposed)

One SQLite database, WAL mode, foreign keys on, explicit `busy_timeout`.

- `events` is append-only. Update/delete are not exposed through any API; a
  `BEFORE UPDATE/DELETE` trigger raises, so a bug cannot rewrite history. Purge
  drops whole time ranges in a maintenance transaction that is itself an event.
- Derived tables (`event_relations`, `operations`, ...) may be rebuilt.
- `schema_version` table with forward-only migrations, each tested on a copy of a
  populated v(N-1) database.
- Two-layer model: raw event layer vs operation-context layer. An event may
  belong to zero, one, or many contexts, enforced as many-to-many — never a
  foreign key from `events` to `operations`.

## Alternatives considered

- Append-only JSONL + side index: simpler durability, no transactional
  multi-table consistency, no query planner.
- Two databases (raw vs derived): cleaner isolation, cross-db joins and a second
  durability story.
- Embedded KV (sled/redb): no SQL, harder ad-hoc auditing.

## Consequences

- Sensitive-value redaction happens before insert; unredacted form is never
  persisted by default.
- Retention policy is explicit and itself audited.

## Open questions

- `events.payload` as JSON text vs normalised columns per event type: Phase 1
  decision informed by 0-H query latency.
