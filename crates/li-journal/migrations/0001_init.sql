-- 0001_init.sql — base journal schema.
--
-- Every shape below is a Phase 0 gate, not a preference. The gates that this
-- file is the *enforcement* of:
--
--   * `events` is append-only. UPDATE and DELETE are refused by triggers, so a
--     bug cannot rewrite history. Purge (ADR 0003) is not implemented here; a
--     future retention sweep must drop and recreate these triggers inside one
--     transaction that is itself recorded as an event.
--   * `events` carries NO `operation_id` (0-H §8.3.1). Membership is the
--     many-to-many `event_operations`, so one event can be in zero, one or
--     several operations, at different levels.
--   * `(pid, starttime)` is never PRIMARY KEY or UNIQUE (0-H §8.3.2, 0-E H2:
--     20/20 same-tick collisions). `incarnation_seq` disambiguates and
--     `ambiguous = 1` is sticky.
--   * `event_relations.kind` is CHECKed to three values, so `caused_by` is not
--     spellable even by hand-written SQL (0-H §8.1.5).
--   * every foreign-key child column has an index on its prefix (0-H §8.3.3:
--     a missing one turned a 65 ms trim into 75 s).
--   * the JSON expression index on `payload->>'$.path'` is created HERE, in the
--     first migration, because 0-H §7.4 measured 2664 us -> 16.6 us (160x) for
--     the query shape it serves and ADR 0003 makes late addition expensive.

CREATE TABLE schema_version (
    version     INTEGER PRIMARY KEY,
    applied_at  TEXT    NOT NULL,
    description TEXT    NOT NULL,
    -- checksum of the migration body: an edited-then-reapplied migration is
    -- detected instead of silently diverging between hosts
    checksum    TEXT    NOT NULL
) STRICT;

-- One row per shell / pty / script lifetime.
CREATE TABLE sessions (
    session_id      TEXT PRIMARY KEY,
    -- a subshell or a `li run` under a session; ON DELETE SET NULL because a
    -- session row is bookkeeping, not a fact that other facts depend on
    parent_session  TEXT REFERENCES sessions (session_id) ON DELETE SET NULL,
    adapter         TEXT NOT NULL,
    adapter_version TEXT,
    shell_pid       INTEGER,
    started_wall    TEXT NOT NULL,
    started_mono_ns INTEGER NOT NULL,
    ended_wall      TEXT,
    ended_mono_ns   INTEGER,
    cwd             TEXT,
    host            TEXT,
    -- `ts_mono_ns` restarts near zero across a reboot (li-proto `Event`), so a
    -- boot identity is what makes two events comparable at all
    boot_id         TEXT
) STRICT;

CREATE INDEX idx_sessions_parent ON sessions (parent_session);
CREATE INDEX idx_sessions_started ON sessions (started_mono_ns);

-- One row per *incarnation* of a pid. `(pid, starttime)` alone is not unique on
-- purpose; see the header.
CREATE TABLE processes (
    process_row_id     INTEGER PRIMARY KEY,
    pid                INTEGER NOT NULL,
    starttime          INTEGER NOT NULL,
    incarnation_seq    INTEGER NOT NULL DEFAULT 0,
    comm               TEXT,
    ppid               INTEGER,
    session_id         TEXT REFERENCES sessions (session_id) ON DELETE SET NULL,
    first_seen_mono_ns INTEGER NOT NULL,
    last_seen_mono_ns  INTEGER,
    exited_mono_ns     INTEGER,
    -- 1 when this key was seen carrying two different descriptions:
    -- attribution through the row must degrade to `unknown`, never guess
    ambiguous          INTEGER NOT NULL DEFAULT 0 CHECK (ambiguous IN (0, 1)),
    UNIQUE (pid, starttime, incarnation_seq)
) STRICT;

CREATE INDEX idx_processes_key ON processes (pid, starttime);
CREATE INDEX idx_processes_session ON processes (session_id);

-- The raw fact layer. Append-only.
CREATE TABLE events (
    -- journal-assigned total order of *arrival*. This, not `sequence`, is the
    -- watermark a consumer acknowledges (C2: per-event Ack is not delivery
    -- proof). AUTOINCREMENT so a rowid is never reused.
    ingest_seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    -- the only key any receiver may drop a row on (C3 / 0-C T7)
    event_id        TEXT    NOT NULL UNIQUE,
    schema_version  INTEGER NOT NULL,
    source          TEXT    NOT NULL,
    session_id      TEXT REFERENCES sessions (session_id) ON DELETE SET NULL,
    process_row_id  INTEGER REFERENCES processes (process_row_id) ON DELETE SET NULL,
    -- producer-assigned, counts within a session; a gap is data, not an error
    sequence        INTEGER NOT NULL,
    ts_mono_ns      INTEGER NOT NULL,
    -- RFC 3339 UTC, milliseconds, `Z`, fixed 24 bytes, so a lexicographic sort
    -- is a time sort
    ts_wall         TEXT    NOT NULL CHECK (length(ts_wall) = 24),
    -- open vocabulary: an unknown `type` is stored, never refused (0-C)
    type            TEXT    NOT NULL CHECK (length(type) BETWEEN 1 AND 64),
    -- coarse class the dedup layer keys on; not CHECKed because adding a class
    -- must not need a migration
    type_class      TEXT    NOT NULL,
    payload         TEXT    NOT NULL CHECK (json_valid(payload)),
    evidence_status TEXT    NOT NULL
                    CHECK (evidence_status IN ('observed', 'declared', 'inferred')),
    -- dedup bookkeeping; a fingerprint match may only attach here, never
    -- delete or overwrite a row (0-H §8.1.1)
    dedup_group     TEXT,
    duplicate_of    TEXT REFERENCES events (event_id),
    -- which rules fired, so a false positive can be audited after the fact
    redaction_rules TEXT,
    -- linkable 64-bit FNV-1a of the first masked secret: answers "the same
    -- credential was reused", nothing more
    redaction_digest TEXT,
    fp_type_class   TEXT,
    fp_subject      TEXT,
    fp_bucket_ns    INTEGER,
    -- writer clock at insert, for ingest-rate and lag diagnostics
    ingested_mono_ns INTEGER NOT NULL
) STRICT;

-- Append-only, at the database layer. Statement-level: one trigger refuses any
-- UPDATE, whether it touches one row or all of them.
CREATE TRIGGER events_append_only_update BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'append_only: events.UPDATE is rejected');
END;

CREATE TRIGGER events_append_only_delete BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'append_only: events.DELETE is rejected');
END;

CREATE INDEX idx_events_session_seq ON events (session_id, sequence);
CREATE INDEX idx_events_time ON events (ts_mono_ns);
CREATE INDEX idx_events_type_time ON events (type, ts_mono_ns);
CREATE INDEX idx_events_process ON events (process_row_id);
CREATE INDEX idx_events_dedup ON events (dedup_group);
-- FK child column; partial because the column is NULL for almost every event
CREATE INDEX idx_events_duplicate_of ON events (duplicate_of)
    WHERE duplicate_of IS NOT NULL;
CREATE INDEX idx_events_fingerprint ON events (fp_type_class, session_id, fp_bucket_ns);

-- The index this migration exists to carry (0-H §7.4, 160x on the file-trail
-- query). Full, not partial: a partial index is only usable when the planner
-- can prove the query implies its WHERE clause, so a range or LIKE query would
-- silently fall back to a scan — the one outcome the measurement forbids.
CREATE INDEX idx_events_payload_path ON events (json_extract(payload, '$.path'));

-- Operation-context layer. Derived: rebuildable from events + rules.
CREATE TABLE operations (
    operation_id    TEXT PRIMARY KEY,
    session_id      TEXT REFERENCES sessions (session_id) ON DELETE SET NULL,
    label           TEXT NOT NULL,
    intent          TEXT,
    state           TEXT NOT NULL DEFAULT 'open'
                    CHECK (state IN ('open', 'closed', 'abandoned', 'recovered')),
    started_mono_ns INTEGER NOT NULL,
    ended_mono_ns   INTEGER
) STRICT;

CREATE INDEX idx_operations_session ON operations (session_id);

-- Many-to-many, with per-edge credibility. There is no sentinel "no operation"
-- row: an event in no operation simply has no row here.
CREATE TABLE event_operations (
    event_id         TEXT NOT NULL REFERENCES events (event_id) ON DELETE CASCADE,
    operation_id     TEXT NOT NULL REFERENCES operations (operation_id) ON DELETE CASCADE,
    -- which rule attached it and at what level, so the claim is re-derivable
    attached_by      TEXT NOT NULL,
    level            TEXT NOT NULL
                     CHECK (level IN ('confirmed', 'strong', 'probable', 'weak', 'unknown')),
    attached_mono_ns INTEGER NOT NULL,
    detached_mono_ns INTEGER,
    PRIMARY KEY (event_id, operation_id)
) STRICT;

CREATE INDEX idx_event_operations_op ON event_operations (operation_id, level);

CREATE TABLE event_relations (
    relation_id         INTEGER PRIMARY KEY AUTOINCREMENT,
    from_event          TEXT NOT NULL REFERENCES events (event_id) ON DELETE CASCADE,
    to_event            TEXT NOT NULL REFERENCES events (event_id) ON DELETE CASCADE,
    -- exactly three kinds: `caused_by` is not spellable (0-H §8.1.5)
    kind                TEXT NOT NULL
                        CHECK (kind IN ('observed_within', 'spawned_by', 'modified_during')),
    level               TEXT NOT NULL
                        CHECK (level IN ('confirmed', 'strong', 'probable', 'weak', 'unknown')),
    -- the rule that fired, so the level can always be recomputed from the row
    rule                TEXT NOT NULL,
    inferred_at_mono_ns INTEGER NOT NULL,
    CHECK (from_event <> to_event),
    UNIQUE (from_event, to_event, kind)
) STRICT;

CREATE INDEX idx_relations_from ON event_relations (from_event, kind);
CREATE INDEX idx_relations_to ON event_relations (to_event, kind);

-- Why a relation exists: one row per corroborating source.
CREATE TABLE evidence (
    evidence_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    relation_id     INTEGER NOT NULL
                    REFERENCES event_relations (relation_id) ON DELETE CASCADE,
    source          TEXT NOT NULL,
    source_class    TEXT NOT NULL
                    CHECK (source_class IN ('kernel_proc', 'kernel_fs', 'declared')),
    evidence_status TEXT NOT NULL
                    CHECK (evidence_status IN ('observed', 'declared', 'inferred')),
    -- 0 when the claim was derived from another source, i.e. not independent
    independent     INTEGER NOT NULL DEFAULT 1 CHECK (independent IN (0, 1)),
    confidence      REAL,
    detail          TEXT
) STRICT;

CREATE INDEX idx_evidence_relation ON evidence (relation_id);

CREATE TABLE checkpoints (
    checkpoint_id TEXT PRIMARY KEY,
    session_id    TEXT REFERENCES sessions (session_id) ON DELETE SET NULL,
    operation_id  TEXT REFERENCES operations (operation_id) ON DELETE SET NULL,
    created_wall  TEXT NOT NULL,
    kind          TEXT NOT NULL
                  CHECK (kind IN ('snapper', 'btrfs', 'git', 'manual', 'none')),
    reference     TEXT,
    file_count    INTEGER,
    note          TEXT
) STRICT;

CREATE INDEX idx_checkpoints_session ON checkpoints (session_id);
CREATE INDEX idx_checkpoints_operation ON checkpoints (operation_id);

CREATE TABLE recovery_plans (
    plan_id       TEXT PRIMARY KEY,
    operation_id  TEXT REFERENCES operations (operation_id) ON DELETE CASCADE,
    checkpoint_id TEXT REFERENCES checkpoints (checkpoint_id) ON DELETE SET NULL,
    created_wall  TEXT NOT NULL,
    -- a plan is data. Phase 1 has no executor (0-F never exercised rollback).
    state         TEXT NOT NULL DEFAULT 'proposed'
                  CHECK (state IN ('proposed', 'approved', 'rejected', 'executed', 'failed')),
    steps_json    TEXT NOT NULL CHECK (json_valid(steps_json)),
    risk          TEXT CHECK (risk IN ('none', 'low', 'medium', 'high')),
    reversible    INTEGER NOT NULL DEFAULT 1 CHECK (reversible IN (0, 1))
) STRICT;

CREATE INDEX idx_recovery_plans_operation ON recovery_plans (operation_id);
CREATE INDEX idx_recovery_plans_checkpoint ON recovery_plans (checkpoint_id);

CREATE TABLE approvals (
    approval_id TEXT PRIMARY KEY,
    plan_id     TEXT NOT NULL REFERENCES recovery_plans (plan_id) ON DELETE CASCADE,
    decision    TEXT NOT NULL CHECK (decision IN ('approve', 'reject')),
    decided_wall TEXT NOT NULL,
    -- who approved: uid plus the auth path, never a bare "user"
    uid         INTEGER NOT NULL,
    auth_method TEXT NOT NULL CHECK (auth_method IN ('tty', 'polkit', 'none')),
    scope_hash  TEXT NOT NULL,
    note        TEXT
) STRICT;

CREATE INDEX idx_approvals_plan ON approvals (plan_id);

-- What each adapter claims it can do. `capabilities` that were never verified
-- must say `unknown`; this table records the claim for audit, it does not
-- validate it (spec C12: capability probe, not distro name).
CREATE TABLE adapter_registry (
    adapter       TEXT PRIMARY KEY,
    version       TEXT,
    capabilities  TEXT NOT NULL CHECK (json_valid(capabilities)),
    verified_wall TEXT,
    notes         TEXT
) STRICT;
