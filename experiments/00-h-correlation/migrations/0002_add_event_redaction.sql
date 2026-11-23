-- v1 -> v2: additive only, so an existing 100k-row database upgrades in place.
--
-- Added because 0-H's redaction experiment showed that storing the *rule set*
-- that fired is necessary to audit a false positive, and that the semantic
-- fingerprint must be persisted to make dedup replayable after a restart.

ALTER TABLE events ADD COLUMN command_text TEXT;
ALTER TABLE events ADD COLUMN redaction_rules TEXT;

CREATE INDEX idx_events_fingerprint ON events (dedup_group, ts_mono_ns);

-- Sessions gained the capability snapshot that was used, because attribute
-- availability differs per adapter version and a 200 ms poller cannot claim
-- what a hook can.
ALTER TABLE sessions ADD COLUMN capability_json TEXT;
