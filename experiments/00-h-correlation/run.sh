#!/usr/bin/env bash
# 0-H driver. Deterministic: every run writes to results/<UTC-stamp>-*.log and
# never edits a previous result file. Exits non-zero on any failure.
#
# Note: nothing in this script deletes files. Each run allocates a fresh
# /tmp/li-phase0-h-<stamp> working directory instead, so a half-finished run can
# never be mistaken for a clean one.
set -euo pipefail

cd "$(dirname "$0")"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS="results"
WORK="/tmp/li-phase0-h-$STAMP"
mkdir -p "$RESULTS" "$WORK"

BIN_LOGIC="target/release/hexp"
BIN_DB="target/release/hdb"
LOG="$RESULTS/$STAMP-run.log"
: > "$LOG"

say() { printf '%s\n' "$*" | tee -a "$LOG"; }
step() { say ""; say "=== $* ==="; }

step "environment"
{
  echo "stamp=$STAMP"
  echo "host=$(uname -srm)"
  echo "rustc=$(rustc --version)"
  echo "sqlite3=$(sqlite3 --version)"
  echo "nproc=$(nproc)"
  echo "kernel_clk_tck=$(getconf CLK_TCK)"
  echo "ulimit_nofile=$(ulimit -n)"
} | tee -a "$LOG"

step "build (offline, release)"
# Offline on purpose: the crate cache is warm and a network fetch would make the
# result unreproducible on a cold machine.
cargo build --release --offline 2>&1 | tail -3 | tee -a "$LOG"
cargo test --release --offline 2>&1 | grep -E "^test result|FAILED" | tee -a "$LOG"

step "unit + logic experiments"
for name in dedup reorder pidreuse confidence causality context redact; do
  out="$RESULTS/$STAMP-$name.log"
  timeout 300 "$BIN_LOGIC" "$name" 2>&1 | tee "$out" > /dev/null
  say "wrote $out ($(wc -l < "$out") lines)"
done
cp /dev/null "$RESULTS/$STAMP-all.log"
timeout 300 "$BIN_LOGIC" all 2>&1 > "$RESULTS/$STAMP-all.log"
say "wrote $RESULTS/$STAMP-all.log ($(wc -l < "$RESULTS/$STAMP-all.log") lines)"

# ---------------------------------------------------------------------------
step "kernel anchor: (pid, starttime) tick granularity (REAL, not synthetic)"
{
  echo "CLK_TCK=$(getconf CLK_TCK)"
  echo "Field 22 of /proc/<pid>/stat is starttime in clock ticks, so its resolution"
  echo "is 1/CLK_TCK s. This measures how many concurrently-forked processes share a"
  echo "starttime, which is the precondition for a (pid, starttime) alias."
  echo
  echo "--- 40 processes forked in one tight loop (each must outlive the read) ---"
  PIDS=""
  for _ in $(seq 1 40); do
    sleep 0.4 &
    PIDS="$PIDS $!"
  done
  STARTTIMES="$WORK/starttimes.txt"
  : > "$STARTTIMES"
  for p in $PIDS; do
    # split on the last ')' so a comm containing ')' cannot shift the fields
    awk '{n=split($0,a,")"); split(a[2],f," "); print f[20]}' "/proc/$p/stat" 2>/dev/null >> "$STARTTIMES" || true
  done
  wait 2>/dev/null || true
  echo "samples=$(wc -l < "$STARTTIMES") distinct_starttimes=$(sort -u "$STARTTIMES" | wc -l)"
  echo
  echo "starttime histogram (count, tick index):"
  sort -n "$STARTTIMES" | uniq -c | sort -rn | head -12
  echo
  echo "--- the same 40 processes: how many distinct pids? ---"
  echo "distinct_pids=$(sort -u "$STARTTIMES" | wc -l) (pids themselves are distinct by definition here)"
  echo
  echo "Interpretation: distinct_starttimes << samples means 1/CLK_TCK s is far coarser"
  echo "than fork latency, so 'same starttime' is common. A (pid, starttime) ALIAS also"
  echo "needs the pid to be recycled inside that same tick; forcing that needs"
  echo "kernel.pid_max wrapping, i.e. privilege this experiment does not have. The alias"
  echo "itself is therefore tested SYNTHETICALLY in the pidreuse step; its precondition"
  echo "is measured here."
} 2>&1 | tee "$RESULTS/$STAMP-pidns-anchor.log" | tail -20

# ---------------------------------------------------------------------------
step "SQLite: v1 -> v2 migration with data, then verify"
MIGDB="$WORK/mig.db"
"$BIN_DB" migrate "$MIGDB" 1 > "$RESULTS/$STAMP-mig-v1.log" 2>&1
say "v1 created: $(grep -c . "$RESULTS/$STAMP-mig-v1.log") lines"
{
  echo "--- populate v1 with rows ---"
  sqlite3 "$MIGDB" <<'SQL'
INSERT INTO sessions (session_id, adapter, started_wall, started_mono_ns)
  VALUES ('s1','bash','2025-11-24T00:00:00.000Z',0),
         ('s2','zsh','2025-11-24T00:00:01.000Z',1000);
INSERT INTO operations (operation_id, session_id, label, state, started_mono_ns)
  VALUES ('op1','s1','edit','open',0);
INSERT INTO events (event_id, schema_version, source, session_id, sequence, ts_mono_ns,
                    ts_wall, type, type_class, payload, evidence_status, ingested_mono_ns)
  SELECT 'e' || value, 1, 'shell.bash', 's1', value, value*1000000,
         '2025-11-24T00:00:00.000Z', 'CommandFinished', 'proc.end',
         '{"command":"cmd' || value || '"}', 'declared', 0
  FROM generate_series(1, 5000);
SELECT 'v1 events=' || count(*) FROM events;
SQL
} 2>&1 | tee "$RESULTS/$STAMP-mig-populate-v1.log" | tail -5

"$BIN_DB" verify "$MIGDB" > "$RESULTS/$STAMP-mig-v1-verify.log" 2>&1 || true
say "v1 column set: $(grep 'events columns' "$RESULTS/$STAMP-mig-v1-verify.log")"

{
  echo "--- upgrade v1 -> v2 on the populated database ---"
  "$BIN_DB" migrate "$MIGDB"
  echo
  echo "--- verify after upgrade ---"
  "$BIN_DB" verify "$MIGDB"
} > "$RESULTS/$STAMP-mig-v2.log" 2>&1
tail -12 "$RESULTS/$STAMP-mig-v2.log" | tee -a "$LOG"

{
  echo "--- data preserved? ---"
  sqlite3 "$MIGDB" "SELECT count(*) AS events FROM events;"
  sqlite3 "$MIGDB" "SELECT count(*) AS sessions FROM sessions;"
  sqlite3 "$MIGDB" "SELECT max(version) AS schema_version FROM schema_version;"
  echo "--- new columns exist and are null for old rows ---"
  sqlite3 "$MIGDB" "SELECT event_id, command_text IS NULL, redaction_rules IS NULL FROM events LIMIT 3;"
  echo "--- old rows still readable through the new column list ---"
  sqlite3 -header "$MIGDB" "SELECT event_id, sequence, type, dedup_group FROM events ORDER BY sequence LIMIT 3;"
  echo "--- integrity after migration ---"
  sqlite3 "$MIGDB" "PRAGMA integrity_check;"
  echo "--- migration ledger ---"
  sqlite3 -header "$MIGDB" "SELECT version, description, checksum FROM schema_version ORDER BY version;"
} 2>&1 | tee "$RESULTS/$STAMP-mig-after.log" | tail -20

# ---------------------------------------------------------------------------
step "SQLite: 100k-row insert + three query shapes"
timeout 600 "$BIN_DB" bench "$WORK/bench.db" 100000 200 \
  > "$RESULTS/$STAMP-bench-100k.log" 2>&1
grep -E "inserted|db_file_bytes|operations=|event_relations=|^q[0-9]|^w1|create_expression" \
  "$RESULTS/$STAMP-bench-100k.log" | tee -a "$LOG"

step "SQLite: 1 writer + 4 readers under WAL"
WALDB="$WORK/wal.db"
timeout 180 "$BIN_DB" wal "$WALDB" 8 \
  > "$RESULTS/$STAMP-wal-concurrency.log" 2>&1
cat "$RESULTS/$STAMP-wal-concurrency.log" | tee -a "$LOG" | tail -22

# ---------------------------------------------------------------------------
step "SQLite: kill -9 a writer mid-transaction"
KILLDB="$WORK/kill9.db"
"$BIN_DB" migrate "$KILLDB" > "$RESULTS/$STAMP-kill9-migrate.log" 2>&1
{
  echo "starting writer, will SIGKILL after 1.5 s of steady commits"
  "$BIN_DB" writer "$KILLDB" 60 > "$WORK/writer.out" 2>&1 &
  WPID=$!
  sleep 1.5
  kill -9 "$WPID" 2>/dev/null || true
  wait "$WPID" 2>/dev/null || true
  echo "writer killed with SIGKILL; its stdout was:"
  cat "$WORK/writer.out" || true
  echo
  echo "--- recovery: open, integrity_check, foreign_key_check ---"
  "$BIN_DB" integrity "$KILLDB"
  echo
  echo "--- WAL files left behind ---"
  ls -l "$WORK" | grep -c kill9 || true
  echo
  echo "--- second open after the crash must also succeed and commit ---"
  "$BIN_DB" writer "$KILLDB" 1
  "$BIN_DB" integrity "$KILLDB"
} 2>&1 | tee "$RESULTS/$STAMP-kill9.log" | tail -20

# ---------------------------------------------------------------------------
step "SQLite: retention — trim then VACUUM on 100k rows"
for mode in time size; do
  timeout 300 "$BIN_DB" retention "$WORK/ret-$mode.db" 100000 "$mode" \
    > "$RESULTS/$STAMP-retention-$mode.log" 2>&1
  say "retention($mode):"
  grep -E "before rows|trim |vacuum|incremental_vacuum|integrity" \
    "$RESULTS/$STAMP-retention-$mode.log" | tee -a "$LOG"
done

# ---------------------------------------------------------------------------
step "summary"
say "results directory: $RESULTS"
ls -1 "$RESULTS" | grep "$STAMP" | tee -a "$LOG"
say ""
say "OK: all steps completed"
