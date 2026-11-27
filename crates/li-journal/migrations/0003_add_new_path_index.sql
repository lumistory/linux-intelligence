-- 0003 (P1-28): index `$.new_path` so rename destination queries via
-- `li query --path` / `Reader::query_events` use an expression index instead
-- of falling back to a full table scan.

CREATE INDEX idx_events_payload_new_path ON events (json_extract(payload, '$.new_path'));
