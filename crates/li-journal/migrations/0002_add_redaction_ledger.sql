-- 0002 — additive only, so a populated v1 database upgrades in place.
--
-- 0-H §7.6 and the redaction path in this crate both need to know *how many*
-- rules fired for one event, not just which ones: a row masked by three rules
-- is a different audit story from a row masked by one, and `redaction_rules`
-- alone cannot distinguish "one rule" from "the same rule twice".
--
-- `ALTER TABLE ADD COLUMN` with a NULL default is O(1) in SQLite (it does not
-- rewrite the table), which is exactly why the migration series is additive:
-- 0-H §7.1 upgraded 5000 live rows this way with zero loss.

ALTER TABLE events ADD COLUMN redaction_hits INTEGER;

-- Backfill is deliberately *not* attempted: the historical rows did not record
-- the count, and inventing one would be a fabricated fact in an append-only
-- table. NULL means "unknown", which is the honest value.
CREATE INDEX idx_events_redacted ON events (redaction_digest)
    WHERE redaction_digest IS NOT NULL;
