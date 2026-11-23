-- 0-H prototype schema, v1.
--
-- Design rules that the experiments below exist to defend:
--   * events carry NO operation_id: membership is many-to-many and optional.
--   * (pid, starttime) is NOT a unique key: `incarnation_seq` disambiguates
--     collisions, and `ambiguous` marks the pair as unusable for merging.
--   * every derived claim is a row in `event_relations` with a `rule` string and
--     a `level`, never a bare boolean column on `events`.
--   * relation kinds are constrained by CHECK: there is no way to store
--     "caused_by" even if a caller tries.

CREATE TABLE schema_version (
    version     INTEGER PRIMARY KEY,
    applied_at  TEXT    NOT NULL,
    description TEXT    NOT NULL,
    -- checksum of the migration body, so an edited-then-reapplied migration is
    -- detected instead of silently diverging between hosts
    checksum    TEXT    NOT NULL
);

CREATE TABLE sessions (
    session_id     TEXT PRIMARY KEY,
    parent_session TEXT REFERENCES sessions(session_id) ON DELETE SET NULL,
    adapter        TEXT NOT NULL,
    adapter_version TEXT,
    shell_pid      INTEGER,
    started_wall   TEXT NOT NULL,
    started_mono_ns INTEGER NOT NULL,
    ended_wall     TEXT,
    ended_mono_ns  INTEGER,
    cwd            TEXT,
    host           TEXT
);

-- One row per *incarnation*. (pid, starttime) is unique per row only because
-- incarnation_seq differs; that is exactly the collision 0-E measured.
CREATE TABLE processes (
    process_row_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    pid              INTEGER NOT NULL,
    starttime        INTEGER NOT NULL,
    incarnation_seq  INTEGER NOT NULL DEFAULT 0,
    comm             TEXT,
    ppid             INTEGER,
    argv_hash        INTEGER,
    session_id       TEXT REFERENCES sessions(session_id) ON DELETE SET NULL,
    first_seen_mono_ns INTEGER NOT NULL,
    last_seen_mono_ns  INTEGER,
    exited_mono_ns     INTEGER,
    -- 1 when the (pid,starttime) key was observed carrying two different
    -- descriptions: attribution through this row must degrade to Unknown.
    ambiguous        INTEGER NOT NULL DEFAULT 0 CHECK (ambiguous IN (0, 1)),
    UNIQUE (pid, starttime, incarnation_seq)
);

CREATE INDEX idx_processes_key ON processes (pid, starttime);
CREATE INDEX idx_processes_session ON processes (session_id);

CREATE TABLE events (
    event_id        TEXT PRIMARY KEY,
    schema_version  INTEGER NOT NULL,
    source          TEXT    NOT NULL,
    session_id      TEXT REFERENCES sessions(session_id) ON DELETE SET NULL,
    process_row_id  INTEGER REFERENCES processes(process_row_id) ON DELETE SET NULL,
    sequence        INTEGER NOT NULL,
    ts_mono_ns      INTEGER NOT NULL,
    ts_wall         TEXT    NOT NULL,
    type            TEXT    NOT NULL,
    type_class      TEXT    NOT NULL,
    payload         TEXT    NOT NULL CHECK (json_valid(payload)),
    evidence_status TEXT    NOT NULL
                    CHECK (evidence_status IN ('observed', 'declared', 'inferred')),
    -- dedup bookkeeping, filled by the ingest path
    dedup_group     TEXT,
    duplicate_of    TEXT REFERENCES events(event_id) ON DELETE SET NULL,
    -- redaction bookkeeping: the digest is linkable, the secret is not stored
    redaction_digest TEXT,
    ingested_mono_ns INTEGER NOT NULL
);

CREATE INDEX idx_events_session_seq ON events (session_id, sequence);
CREATE INDEX idx_events_time ON events (ts_mono_ns);
CREATE INDEX idx_events_type_time ON events (type, ts_mono_ns);
CREATE INDEX idx_events_process ON events (process_row_id);
CREATE INDEX idx_events_dedup ON events (dedup_group);

-- Optional, many-to-many, retractable.
CREATE TABLE operations (
    operation_id   TEXT PRIMARY KEY,
    session_id     TEXT REFERENCES sessions(session_id) ON DELETE SET NULL,
    label          TEXT NOT NULL,
    intent         TEXT,
    state          TEXT NOT NULL DEFAULT 'open'
                   CHECK (state IN ('open', 'closed', 'abandoned', 'recovered')),
    started_mono_ns INTEGER NOT NULL,
    ended_mono_ns   INTEGER
);

CREATE INDEX idx_operations_session ON operations (session_id);

-- The join table that makes "one event, several operations" and "one event, no
-- operation" both representable without a sentinel operation row.
CREATE TABLE event_operations (
    event_id       TEXT NOT NULL REFERENCES events(event_id) ON DELETE CASCADE,
    operation_id   TEXT NOT NULL REFERENCES operations(operation_id) ON DELETE CASCADE,
    attached_by    TEXT NOT NULL,
    level          TEXT NOT NULL
                   CHECK (level IN ('Confirmed','Strong','Probable','Weak','Unknown')),
    attached_mono_ns INTEGER NOT NULL,
    detached_mono_ns INTEGER,
    PRIMARY KEY (event_id, operation_id)
);

CREATE INDEX idx_event_operations_op ON event_operations (operation_id, level);

CREATE TABLE event_relations (
    relation_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    from_event    TEXT NOT NULL REFERENCES events(event_id) ON DELETE CASCADE,
    to_event      TEXT NOT NULL REFERENCES events(event_id) ON DELETE CASCADE,
    -- exactly three kinds; `caused_by` is not spellable
    kind          TEXT NOT NULL
                  CHECK (kind IN ('observed_within', 'spawned_by', 'modified_during')),
    level         TEXT NOT NULL
                  CHECK (level IN ('Confirmed','Strong','Probable','Weak','Unknown')),
    -- which rule in the classifier fired: makes the level auditable
    rule          TEXT NOT NULL,
    inferred_at_mono_ns INTEGER NOT NULL,
    CHECK (from_event <> to_event),
    UNIQUE (from_event, to_event, kind)
);

CREATE INDEX idx_relations_from ON event_relations (from_event, kind);
CREATE INDEX idx_relations_to ON event_relations (to_event, kind);

-- Why a relation exists, one row per corroborating source.
CREATE TABLE evidence (
    evidence_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    relation_id    INTEGER NOT NULL REFERENCES event_relations(relation_id) ON DELETE CASCADE,
    source         TEXT NOT NULL,
    source_class   TEXT NOT NULL CHECK (source_class IN ('kernel_proc','kernel_fs','declared')),
    evidence_status TEXT NOT NULL
                   CHECK (evidence_status IN ('observed','declared','inferred')),
    -- 0 when the claim was derived from another source (not independent)
    independent    INTEGER NOT NULL DEFAULT 1 CHECK (independent IN (0, 1)),
    confidence     REAL,
    detail         TEXT
);

CREATE INDEX idx_evidence_relation ON evidence (relation_id);

CREATE TABLE checkpoints (
    checkpoint_id  TEXT PRIMARY KEY,
    session_id     TEXT REFERENCES sessions(session_id) ON DELETE SET NULL,
    operation_id   TEXT REFERENCES operations(operation_id) ON DELETE SET NULL,
    created_wall   TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('snapper','btrfs','git','manual','none')),
    reference      TEXT,
    file_count     INTEGER,
    note           TEXT
);

CREATE TABLE recovery_plans (
    plan_id        TEXT PRIMARY KEY,
    operation_id   TEXT REFERENCES operations(operation_id) ON DELETE CASCADE,
    checkpoint_id  TEXT REFERENCES checkpoints(checkpoint_id) ON DELETE SET NULL,
    created_wall   TEXT NOT NULL,
    -- a plan is data, never executed by this prototype
    state          TEXT NOT NULL DEFAULT 'proposed'
                   CHECK (state IN ('proposed','approved','rejected','executed','failed')),
    steps_json     TEXT NOT NULL CHECK (json_valid(steps_json)),
    risk           TEXT CHECK (risk IN ('none','low','medium','high')),
    reversible     INTEGER NOT NULL DEFAULT 1 CHECK (reversible IN (0, 1))
);

CREATE TABLE approvals (
    approval_id    TEXT PRIMARY KEY,
    plan_id        TEXT NOT NULL REFERENCES recovery_plans(plan_id) ON DELETE CASCADE,
    decision       TEXT NOT NULL CHECK (decision IN ('approve','reject')),
    decided_wall   TEXT NOT NULL,
    -- who approved: uid plus the auth path, never a bare "user"
    uid            INTEGER NOT NULL,
    auth_method    TEXT NOT NULL CHECK (auth_method IN ('tty','polkit','none')),
    scope_hash     TEXT NOT NULL,
    note           TEXT
);

CREATE INDEX idx_approvals_plan ON approvals (plan_id);

CREATE TABLE adapter_registry (
    adapter        TEXT PRIMARY KEY,
    version        TEXT,
    capabilities   TEXT NOT NULL CHECK (json_valid(capabilities)),
    -- capabilities that were never verified must be "unknown"; enforced by
    -- convention in the adapter loader, recorded here for audit
    verified_wall  TEXT,
    notes          TEXT
);
