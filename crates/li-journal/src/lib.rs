//! SQLite journal: schema, forward-only migrations, append-only writer.
//!
//! TODO(phase1-batch2): migrations/0001 with the expression index on
//! `json_extract(payload,'$.path')` in the **first** migration (0-H measured a
//! 169x penalty for adding it later), statement-level `BEFORE UPDATE/DELETE`
//! triggers that raise, `event_operations(event_id, operation_id, level,
//! attached_by, detached_mono_ns)` with PK `(event_id, operation_id)` and no
//! `events.operation_id` column, and `(pid, starttime)` never UNIQUE.
//! Single writer thread; WAL, `synchronous=NORMAL`, `busy_timeout=5000`,
//! `foreign_keys=ON` per connection.
