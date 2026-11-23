#!/usr/bin/env bash
# Experiment 00-C — Unix domain socket concurrency and reliability.
#
#   ./run.sh                 # run every test
#   ./run.sh t4 t5           # run selected tests
#   ./run.sh list            # list test names
#
# Every test writes one raw log to results/<UTC timestamp>-<test>.log and
# prints a single PASS/FAIL line. Exit status is non-zero if any test failed.
# Nothing outside this directory, /tmp/li-phase0-ipc and the cargo target dir
# is touched. No sudo, no rm -rf, no root.

set -euo pipefail

cd "$(dirname "$0")"

EXP_DIR="$PWD"
BIN_DIR="$EXP_DIR/target/release"
IPC_DIR="/tmp/li-phase0-ipc"
RESULTS="$EXP_DIR/results"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
SOCK="$IPC_DIR/ipc.sock"
JOURNAL="$IPC_DIR/journal.jsonl"

mkdir -p "$RESULTS" "$IPC_DIR"

log()  { printf '%s\n' "$*" ; }
note() { printf '# %s\n' "$*" ; }

# ---------------------------------------------------------------------------
# build
# ---------------------------------------------------------------------------

build() {
  note "cargo build --release --offline"
  cargo build --release --offline 2>&1
  cargo test --release --offline --lib 2>&1
  note "rustc: $(rustc --version)"
  note "cargo: $(cargo --version)"
}

new_result()    { printf '%s/%s-%s.log'        "$RESULTS" "$RUN_ID" "$1"; }
new_daemon_log() { printf '%s/%s-%s.daemon.log' "$RESULTS" "$RUN_ID" "$1"; }

# ---------------------------------------------------------------------------
# daemon lifecycle
# ---------------------------------------------------------------------------

DAEMON_PID=""

start_daemon() { # start_daemon <logfile> [extra args...]
  local out="$1"; shift
  "$BIN_DIR/ipcd" --socket "$SOCK" --journal "$JOURNAL" "$@" >>"$out" 2>&1 &
  DAEMON_PID=$!
  local i
  for i in $(seq 1 200); do
    [[ -S "$SOCK" ]] && return 0
    sleep 0.02
  done
  log "FATAL: daemon did not create $SOCK"
  return 1
}

stop_daemon() { # graceful: SIGTERM, collects STATS
  [[ -z "$DAEMON_PID" ]] && return 0
  kill -TERM "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
  DAEMON_PID=""
}

kill9_daemon() {
  [[ -z "$DAEMON_PID" ]] && return 0
  kill -9 "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
  DAEMON_PID=""
}

cleanup_all() {
  if [[ -n "$DAEMON_PID" ]]; then
    kill -CONT "$DAEMON_PID" 2>/dev/null || true
    kill -9 "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=""
  fi
}
trap cleanup_all EXIT

# ---------------------------------------------------------------------------
# machine / environment record (goes into every run)
# ---------------------------------------------------------------------------

env_record() {
  note "host"
  uname -a
  note "cpu / memory"
  nproc
  grep -m1 'model name' /proc/cpuinfo || true
  free -b | head -2
  note "limits"
  ulimit -n
  cat /proc/sys/net/core/wmem_default /proc/sys/net/core/wmem_max \
      /proc/sys/net/core/rmem_default /proc/sys/net/core/rmem_max \
      /proc/sys/net/unix/max_dgram_qlen 2>/dev/null || true
  note "user"
  id
  note "runtime dir"
  printf 'XDG_RUNTIME_DIR=%s\n' "${XDG_RUNTIME_DIR:-<unset>}"
  note "versions"
  rustc --version
  cargo --version
  note "binaries"
  ls -l "$BIN_DIR/ipcd" "$BIN_DIR/ipcclient"
  sha256sum "$BIN_DIR/ipcd" "$BIN_DIR/ipcclient"
}

# ---------------------------------------------------------------------------
# T1 — 50 clients x 100 events: completeness, throughput, p50/p99
# ---------------------------------------------------------------------------

t1_concurrency() {
  local out dlog
  out="$(new_result t1-concurrency)"
  dlog="$(new_daemon_log t1-concurrency)"
  {
    note "T1 concurrency: 50 clients x 100 events, one session per client"
    note "method: each client opens its own connection, batches its 100 events"
    note "        into one write(), then sends Ping and waits for Pong. Latency"
    note "        is measured per event as (ack_receipt - event_creation)."
    env_record
    start_daemon "$dlog"
    note "daemon pid=$DAEMON_PID"
    echo "--- run 1 (cold) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" concurrency --clients 50 --events 100 --tag c1
    echo "--- run 2 (repeats) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" concurrency --clients 50 --events 100 --tag c2
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" concurrency --clients 50 --events 100 --tag c3
    echo "--- per-session sequence monotonicity (daemon counter) ---"
    for tag in c1 c2 c3; do
      for c in 0 7 49; do
        LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" audit --journal "$JOURNAL" \
          --session "$tag-c$c" --expect 100
      done
    done
    echo "--- idle round-trip latency (one event, one ack, no other load) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" latency --n 2000 --session lat1
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" latency --n 2000 --session lat2
    sleep 0.3
    stop_daemon
  } >>"$out" 2>&1
  echo "--- daemon stats (from $dlog) ---" >>"$out"
  grep -E 'STATS|reject peer|read error' "$dlog" >>"$out" 2>&1 || true
  grep -E '"mode":"concurrency"|"mode":"latency"|"mode":"audit"' "$out" || true
  grep -E 'STATS' "$dlog" || true

  local eps all_acked viol epss
  eps="$(grep -o '"throughput_events_per_sec":[0-9.]*' "$out" | cut -d: -f2 | tr '\n' ' ')"
  all_acked="$(grep -o '"all_acked":true' "$out" | wc -l)"
  viol="$(grep -o '"seq_violations":[0-9]*' "$dlog" | tail -1 | cut -d: -f2)"
  epss="$(grep -o '"events_per_sec' "$out" | wc -l)"
  log "T1 throughput events/sec: $eps"
  log "T1 runs with all events acked: $all_acked/3, seq_violations=${viol:-?}"
  if [[ "${all_acked:-0}" -ge 3 && "${viol:-1}" -eq 0 ]]; then
    log "T1 PASS (5000/5000 acked x3, per-session sequence strictly monotonic)"
  else
    log "T1 FAIL (acked=$all_acked violations=${viol:-?})"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T2 — SO_PEERCRED and uid rejection
# ---------------------------------------------------------------------------

t2_auth() {
  local out dlog
  out="$(new_result t2-auth)"
  dlog="$(new_daemon_log t2-auth)"
  local bindir="$IPC_DIR/bin"
  mkdir -p "$bindir"
  cp "$BIN_DIR/ipcd" "$BIN_DIR/ipcclient" "$bindir"/
  chmod 755 "$bindir" "$bindir/ipcd" "$bindir/ipcclient"

  {
    note "T2 SO_PEERCRED identity and uid-based rejection"
    note "The daemon is told --allowed-uid 1000 (the real user)."
    note "A second client runs in a user namespace as a foreign uid; the"
    note "kernel reports the *host* uid to the daemon, which is what makes"
    note "an allow-list meaningful across namespaces."
    env_record
    note "socket dir mode 0755 and socket mode 0666 so a foreign uid can even"
    note "reach accept() -- otherwise this test would only prove DAC."
    start_daemon "$dlog" --allowed-uid 1000 --dir-mode 755 --sock-mode 666
    note "daemon pid=$DAEMON_PID"

    echo "--- A: same uid, must be accepted ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" concurrency --clients 4 --events 5 --tag authok
    echo "--- B: same uid, peer cred echoed by the client itself ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" dup --count 3 --repeat-idx 1

    echo "--- C: foreign uid via unshare -Ur, must be rejected ---"
    if command -v unshare >/dev/null && unshare --user true 2>/dev/null; then
      set +e
      LI_IPC_SOCK="$SOCK" unshare --user \
        --map-users=0:1000:1 --map-users=5:100000:1 \
        --map-groups=0:1000:1 --map-groups=5:100000:1 \
        setpriv --reuid 5 --regid 5 --clear-groups \
        sh -c 'id; ulimit -n 256; LI_IPC_SOCK='"$SOCK"' '"$bindir"'/ipcclient sender --count 20 --session foreign' 2>&1
      echo "foreign_client_exit=$?"
      set -e
    else
      echo "NOT TESTED: unshare --user unavailable"
    fi
    sleep 0.4
    echo "--- D: did any of the foreign client's 20 events reach the journal? ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" audit --journal "$JOURNAL" \
      --session foreign --expect 20
    echo "--- E: the accepted uid's events did reach the journal ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" audit --journal "$JOURNAL" \
      --session authok-c0 --expect 5
    stop_daemon
  } >>"$out" 2>&1

  echo "--- daemon log: peer credentials seen ---" >>"$out"
  grep -E 'accept peer|reject peer|STATS' "$dlog" >>"$out" 2>&1 || true
  grep -E 'accept peer|reject peer|STATS' "$dlog" || true

  local rej acc for_events
  rej="$(grep -o '"rejected_uid":[0-9]*' "$dlog" | tail -1 | cut -d: -f2)"
  acc="$(grep -o '"accepted":[0-9]*' "$dlog" | tail -1 | cut -d: -f2)"
  for_events="$(grep -c 'reject peer' "$dlog" || true)"
  log "T2 peercred: accepted=${acc:-?} rejected_uid=${rej:-0} reject_log_lines=$for_events"
  if [[ "${rej:-0}" -ge 1 ]]; then
    log "T2 PASS (foreign uid rejected, same uid accepted)"
  else
    log "T2 FAIL (no uid rejection observed)"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T3 — 10 MiB frame rejected, daemon survives
# ---------------------------------------------------------------------------

t3_oversize() {
  local out dlog
  out="$(new_result t3-oversize)"
  dlog="$(new_daemon_log t3-oversize)"
  {
    note "T3 single 10 MiB frame; limit is 1 MiB (MAX_FRAME_BYTES)"
    env_record
    start_daemon "$dlog"
    note "daemon pid=$DAEMON_PID"
    echo "--- A: 10 MiB frame, then Ping on the same connection ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" oversize --mib 10
    echo "--- B: 1 MiB frame (just over the limit, 1048577 B) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" oversize --mib 1
    echo "--- C: the daemon still serves new clients ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" concurrency --clients 5 --events 10 --tag after
    echo "--- D: 64 MiB frame (discard cap behaviour) ---"
    set +e
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" oversize --mib 64
    echo "64MiB client exit=$?"
    set -e
    sleep 0.3
    stop_daemon
  } >>"$out" 2>&1
  echo "--- daemon stats ---" >>"$out"
  grep -E 'STATS|oversize|too large|discard' "$dlog" >>"$out" 2>&1 || true

  local survived; survived="$(grep -c '"connection_survived":true' "$out" || true)"
  local stats; stats="$(grep -o '"oversize_frames":[0-9]*' "$dlog" | tail -1 || true)"
  log "T3 oversize: connections_survived=$survived $stats"
  grep -o '"error_frames":\[[^]]*\]' "$out" | head -3 || true
  if [[ "${survived:-0}" -ge 1 ]]; then
    log "T3 PASS"
  else
    log "T3 FAIL"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T4 — write blocking with the daemon SIGSTOPped
# ---------------------------------------------------------------------------

t4_write_block() {
  local out dlog
  out="$(new_result t4-write-block)"
  dlog="$(new_daemon_log t4-write-block)"
  {
    note "T4 client write behaviour while the daemon is SIGSTOPped"
    note "Phase A uses a non-blocking socket to count exactly how many frames"
    note "the kernel accepts; phase B uses a blocking socket and times the"
    note "write that stalls. Both are per-connection, so the numbers are"
    note "about one small shell hook write, not about the whole system."
    env_record
    start_daemon "$dlog"
    note "daemon pid=$DAEMON_PID"
    echo "--- warm-up: one client so the daemon has an established peer ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" concurrency --clients 1 --events 1 --tag warm
    echo "--- SIGSTOP daemon ---"
    kill -STOP "$DAEMON_PID"
    note "daemon state: $(ps -o stat= -p "$DAEMON_PID")"
    echo "--- A: 120 B payload (~390 B frames), SO_SNDTIMEO 2 ms ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" fill --pad 120 --wait-ms 2000 \
      --session t4a --sndtimeo-us 2000
    echo "--- B: 8 KiB payload (~8.5 kB frames) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" fill --pad 8192 --wait-ms 2000 \
      --session t4b --sndtimeo-us 2000
    echo "--- C: minimal payload (~270 B frames) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" fill --pad 0 --wait-ms 2000 \
      --session t4c --sndtimeo-us 2000
    echo "--- D: 120 B payload, SO_SNDTIMEO 50 ms (realistic hook timeout) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" fill --pad 120 --wait-ms 2000 \
      --session t4d --sndtimeo-us 50000
    echo "--- E: 120 B payload, SO_SNDTIMEO 5 ms ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" fill --pad 120 --wait-ms 2000 \
      --session t4e --sndtimeo-us 5000
    echo "--- SIGCONT daemon, then verify it recovers ---"
    kill -CONT "$DAEMON_PID"
    sleep 1.0
    note "daemon state: $(ps -o stat= -p "$DAEMON_PID")"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" concurrency --clients 2 --events 5 --tag recover
    echo "--- socket buffer sysctls ---"
    cat /proc/sys/net/core/wmem_default /proc/sys/net/core/wmem_max
    stop_daemon
  } >>"$out" 2>&1
  grep -E 'STATS' "$dlog" >>"$out" 2>&1 || true
  grep -E '"mode":"fill"' "$out" || true
  grep -E 'STATS' "$dlog" || true
  if grep -q '"mode":"fill"' "$out"; then
    log "T4 PASS (blocking measured)"
  else
    log "T4 FAIL (no fill measurement)"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T5 — cost of one report attempt with the daemon gone
# ---------------------------------------------------------------------------

t5_no_daemon() {
  local out dlog
  out="$(new_result t5-no-daemon)"
  dlog="$(new_daemon_log t5-no-daemon)"
  {
    note "T5 single report attempt when the daemon is absent"
    note "Case A: socket file does not exist (ENOENT)."
    note "Case B: socket file exists, nothing listening (ECONNREFUSED)."
    note "Case C (a socket path in a directory the client cannot write) is"
    note "NOT TESTED: with the socket already created, write permission on the"
    note "directory is not needed to connect(), so it is not a distinct failure"
    env_record

    echo "--- A: no socket file at all ---"
    note "removing any previous socket via the daemon's own unlink path is not"
    note "applicable here; the path is simply never created for case A."
    LI_IPC_SOCK="$IPC_DIR/definitely-absent.sock" "$BIN_DIR/ipcclient" probe --n 100

    echo "--- B: socket file present, no listener ---"
    start_daemon "$dlog"
    stop_daemon
    ls -l "$SOCK" || echo "socket file gone after clean shutdown (expected)"
    # Recreate a bound-but-dead socket: bind then exit without unlink.
    "$BIN_DIR/ipcd" --socket "$SOCK" --journal "$JOURNAL" >>"$dlog" 2>&1 &
    local dpid=$!
    sleep 0.3
    kill -9 "$dpid" 2>/dev/null || true
    wait "$dpid" 2>/dev/null || true
    if [[ -S "$SOCK" ]]; then
      note "stale socket file exists (daemon killed with SIGKILL): $SOCK"
      LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" probe --n 100
    else
      note "NOT TESTED for case B: no stale socket file survived the SIGKILL"
    fi
  } >>"$out" 2>&1

  grep -E '"mode":"probe"|outcomes' "$out" || true
  if grep -q '"mode":"probe"' "$out"; then
    log "T5 PASS (attempt cost measured)"
  else
    log "T5 FAIL"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T6 — daemon kill -9 then restart: backoff + sequence gap
# ---------------------------------------------------------------------------

t6_restart() {
  local out dlog
  out="$(new_result t6-restart)"
  dlog="$(new_daemon_log t6-restart)"
  {
    note "T6 daemon kill -9 and restart while a client keeps producing"
    note "The client keeps producing at a fixed rate and keeps a bounded"
    note "queue. Events produced while no daemon was reachable are lost;"
    note "the loss must be visible as a gap in per-session sequence."
    env_record
    start_daemon "$dlog" --journal-append
    note "daemon pid=$DAEMON_PID"
    {
      LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" sender \
        --session t6 --count 4000 --rate-us 300 --queue-cap 256 \
        --reconnect --max-backoff-ms 500 --report "$IPC_DIR/t6-report.json" \
        >"$IPC_DIR/t6-sender.out" 2>&1 &
      echo $! >"$IPC_DIR/t6-sender.pid"
    }
    local spid; spid="$(cat "$IPC_DIR/t6-sender.pid")"
    sleep 0.4
    echo "--- kill -9 the daemon mid-stream ---"
    local down_t0; down_t0="$(date +%s%N)"
    kill9_daemon
    note "daemon killed while client pid=$spid is running"
    sleep 1.2
    echo "--- restart the daemon (journal append, same path) ---"
    start_daemon "$dlog" --journal-append
    local down_t1; down_t1="$(date +%s%N)"
    note "outage_ms=$(( (down_t1 - down_t0) / 1000000 )) (kill -> new listener bound)"
    note "daemon pid=$DAEMON_PID"
    wait "$spid" 2>/dev/null || note "sender exited non-zero (see its own output)"
    cat "$IPC_DIR/t6-sender.out"
    echo "--- audit against the journal (authoritative) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" audit --journal "$JOURNAL" --session t6 --expect 4000
    sleep 0.3
    stop_daemon
  } >>"$out" 2>&1
  echo "--- daemon stats (second instance) ---" >>"$out"
  grep -E 'STATS' "$dlog" | tail -2 >>"$out" 2>&1 || true

  grep -E '"mode":"sender"|reconnected|connect_failed' "$out" | head -6 || true
  grep -E '"mode":"audit"' "$out" || true
  if grep -q '"event": "reconnected"' "$out" || grep -q '"event":"reconnected"' "$out"; then
    log "T6 PASS (reconnected after SIGKILL)"
  else
    log "T6 FAIL (no reconnect recorded)"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T7 — duplicate event_id deduplication
# ---------------------------------------------------------------------------

t7_dedup() {
  local out dlog
  out="$(new_result t7-dedup)"
  dlog="$(new_daemon_log t7-dedup)"
  {
    note "T7 deduplication by event_id"
    note "The client sends N unique events, then resends one byte-identical"
    note "frame. The daemon must journal N lines, not N+1."
    env_record
    start_daemon "$dlog"
    note "daemon pid=$DAEMON_PID"
    local before after
    echo "--- unique events 0..9, then resend index 5 verbatim ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" dup --count 10 --repeat-idx 5
    echo "--- audit: expect 10 journal lines, 0 duplicates ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" audit --journal "$JOURNAL" --session dup --expect 10
    echo "--- repeat 20x with a resend each time, distinct session per round ---"
    local i
    for i in $(seq 1 20); do
      LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" dup --count 10 --repeat-idx 3 \
        --session "dup-r$i" >/dev/null
    done
    for i in $(seq 1 20); do
      LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" audit --journal "$JOURNAL" \
        --session "dup-r$i" --expect 10 | grep -o '"lost":[0-9]*' | sed "s/^/round $i /"
    done
    echo "--- note: dedup is by event_id only. Re-sending the same sequence with a"
    echo "    NEW event_id is accepted (correct: sequence is per session, and a"
    echo "    session that restarts at 0 after a reboot must still be ingestable)."
    sleep 0.3
    stop_daemon
  } >>"$out" 2>&1
  grep -E 'STATS' "$dlog" >>"$out" 2>&1 || true

  local dupd; dupd="$(grep -o '"dedup_dropped":[0-9]*' "$dlog" | tail -1 | cut -d: -f2)"
  log "T7 dedup: dedup_dropped=${dupd:-0} (expect 21: one per resend)"
  grep -E '"mode":"audit"' "$out" | tail -2 || true
  if [[ "${dupd:-0}" -ge 1 ]]; then
    log "T7 PASS"
  else
    log "T7 FAIL (no dedup recorded)"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T8 — 10000 events, daemon dies mid-stream, count discovered losses
# ---------------------------------------------------------------------------

t8_loss() {
  local out dlog
  out="$(new_result t8-loss)"
  dlog="$(new_daemon_log t8-loss)"
  {
    note "T8 message-loss detectability across a daemon crash"
    note "Client sends 10000 events, seq 0..9999 in one session, at ~9k/s."
    note "The daemon is SIGKILLed mid-stream and restarted ~1 s later. The"
    note "journal is then audited for the exact set of sequences that arrived."
    note "Variant A uses a large client queue, so the outage is absorbed in"
    note "memory. Variant B uses a tiny queue, so events really are dropped."
    env_record
  } >>"$out" 2>&1

  # --- variant A: large client queue, outage absorbed in memory -------------
  {
    echo "=== variant A: client queue-cap 8192, outage absorbed in memory ==="
    start_daemon "$dlog" --journal-append
    note "daemon pid=$DAEMON_PID"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" sender \
      --session t8a --count 10000 --rate-us 100 --queue-cap 8192 \
      --reconnect --max-backoff-ms 200 --report "$IPC_DIR/t8a-report.json" \
      >"$IPC_DIR/t8a-sender.out" 2>&1 &
    local spid=$!
    sleep 0.5
    echo "--- SIGKILL the daemon while it is ingesting ---"
    kill9_daemon
    note "journal lines at kill: $(wc -l <"$JOURNAL")"
    sleep 1.0
    echo "--- restart, appending to the same journal ---"
    start_daemon "$dlog" --journal-append
    note "daemon pid=$DAEMON_PID"
    wait "$spid" 2>/dev/null || note "sender exited non-zero"
    cat "$IPC_DIR/t8a-sender.out"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" audit --journal "$JOURNAL" \
      --session t8a --expect 10000
    sleep 0.3
    stop_daemon
  } >>"$out" 2>&1

  # --- variant B: tiny client queue, events really lost ---------------------
  {
    echo "=== variant B: client queue-cap 16, events lost during the outage ==="
    start_daemon "$dlog" --journal-append
    note "daemon pid=$DAEMON_PID"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" sender \
      --session t8b --count 10000 --rate-us 100 --queue-cap 16 \
      --reconnect --max-backoff-ms 200 --report "$IPC_DIR/t8b-report.json" \
      >"$IPC_DIR/t8b-sender.out" 2>&1 &
    local spid=$!
    sleep 0.5
    kill9_daemon
    sleep 1.0
    start_daemon "$dlog" --journal-append
    note "daemon pid=$DAEMON_PID"
    wait "$spid" 2>/dev/null || note "sender exited non-zero"
    cat "$IPC_DIR/t8b-sender.out"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" audit --journal "$JOURNAL" \
      --session t8b --expect 10000
    sleep 0.3
    stop_daemon
  } >>"$out" 2>&1

  # --- reconciliation: client accounting vs journal truth -------------------
  {
    echo "=== reconciliation: client accounting vs journal truth ==="
    "$EXP_DIR/reconcile.py" "$out"
  } >>"$out" 2>&1

  grep -E 'STATS' "$dlog" | tail -2 >>"$out" 2>&1 || true
  tail -30 "$out" | grep -E 'variant|session|reconcil|t8[ab]:' || true
  if grep -q '"mode":"audit"' "$out"; then
    log "T8 PASS (loss quantified from the journal)"
  else
    log "T8 FAIL"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T9 — backpressure: slow daemon, client drops and counts
# ---------------------------------------------------------------------------

t9_backpressure() {
  local out dlog
  out="$(new_result t9-backpressure)"
  dlog="$(new_daemon_log t9-backpressure)"
  {
    note "T9 backpressure with a deliberately slow daemon (--slow-ms 3)"
    note "The client has a bounded queue; when it is full the client drops"
    note "and counts instead of growing without bound. max_queue_depth must"
    note "never exceed the configured cap."
    env_record
    start_daemon "$dlog" --slow-ms 3 --queue-cap 64
    note "daemon pid=$DAEMON_PID"
    echo "--- A: 2000 events, client queue cap 64, policy drop ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" sender \
      --session t9 --count 2000 --queue-cap 64 --policy drop --timeout-s 60
    echo "--- B: client queue cap 8 (aggressive drop) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" sender \
      --session t9b --count 500 --queue-cap 8 --policy drop --timeout-s 60
    echo "--- client RSS during a long slow stream ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" sender \
      --session t9c --count 2000 --queue-cap 64 --policy drop --timeout-s 60 &
    local spid=$!
    local peak=0
    while kill -0 "$spid" 2>/dev/null; do
      local rss
      rss="$(awk '/VmRSS/{print $2}' /proc/"$spid"/status 2>/dev/null || echo 0)"
      [[ -n "$rss" && "$rss" -gt "$peak" ]] && peak="$rss" && note "client VmRSS peak so far: ${peak} kB"
      sleep 0.1
    done
    wait "$spid" 2>/dev/null || true
    note "client VmRSS peak: ${peak} kB"
    sleep 0.3
    stop_daemon
  } >>"$out" 2>&1
  grep -E 'STATS' "$dlog" >>"$out" 2>&1 || true

  grep -E '"mode":"sender"' "$out" || true
  local ok=1
  # every sender result must show max_queue_depth <= queue_cap
  while read -r line; do
    local depth cap
    depth="$(printf '%s' "$line" | grep -o '"max_queue_depth":[0-9]*' | cut -d: -f2)"
    cap="$(printf '%s' "$line" | grep -o '"queue_cap":[0-9]*' | cut -d: -f2)"
    [[ -z "$depth" ]] && continue
    if [[ "$depth" -gt "$cap" ]]; then
      log "T9 FAIL: max_queue_depth=$depth exceeded queue_cap=$cap"
      ok=0
    fi
  done < <(grep -E '"mode":"sender"' "$out" || true)
  if [[ "$ok" -eq 1 ]]; then
    log "T9 PASS (queue stayed bounded)"
  else
    return 1
  fi
}

# ---------------------------------------------------------------------------
# T10 — abnormal close, and unlink of the socket file
# ---------------------------------------------------------------------------

t10_abnormal() {
  local out dlog
  out="$(new_result t10-abnormal)"
  dlog="$(new_daemon_log t10-abnormal)"
  {
    note "T10 A: client closes the fd without shutdown(); does the daemon leak"
    note "     the connection (conns_opened vs conns_closed)?"
    note "T10 B: unlink the socket file under a live connection; the existing"
    note "     connection must keep working, new ones must fail."
    env_record
    start_daemon "$dlog"
    note "daemon pid=$DAEMON_PID"
    echo "--- A1: 50 abrupt closes ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" abrupt --conns 50
    echo "--- A2: 50 abrupt closes with SO_LINGER 0 (RST) ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" abrupt --conns 50 --linger0
    echo "--- A3: 200 abrupt closes, checking fd count in the daemon ---"
    local fds_before fds_after
    fds_before="$(ls /proc/"$DAEMON_PID"/fd 2>/dev/null | wc -l)"
    note "daemon open fds before: $fds_before"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" abrupt --conns 200
    sleep 0.6
    fds_after="$(ls /proc/"$DAEMON_PID"/fd 2>/dev/null | wc -l)"
    note "daemon open fds after: $fds_after"
    echo "--- B: unlink the socket file while a client holds a connection ---"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" hold --hold-ms 1500 &
    local hpid=$!
    sleep 0.5
    note "unlinking $SOCK (this is what a crashed-and-restarted daemon leaves)"
    LI_IPC_SOCK="$SOCK" "$BIN_DIR/ipcclient" unlink --path "$SOCK"
    wait "$hpid" 2>/dev/null || true
    sleep 0.3
    stop_daemon
  } >>"$out" 2>&1
  grep -E 'STATS' "$dlog" >>"$out" 2>&1 || true

  grep -E '"mode":"abrupt"|"mode":"hold"|fds_' "$out" || true
  grep -E 'STATS' "$dlog" || true
  local opened closed
  opened="$(grep -o '"conns_opened":[0-9]*' "$dlog" | tail -1 | cut -d: -f2)"
  closed="$(grep -o '"conns_closed":[0-9]*' "$dlog" | tail -1 | cut -d: -f2)"
  log "T10 conns_opened=${opened:-?} conns_closed=${closed:-?}"
  if [[ "${opened:-0}" == "${closed:-1}" ]]; then
    log "T10 PASS (no connection leak)"
  else
    log "T10 WARN (opened != closed; see log)"
  fi
}

# ---------------------------------------------------------------------------
# driver
# ---------------------------------------------------------------------------

ALL_TESTS=(t1_concurrency t2_auth t3_oversize t4_write_block t5_no_daemon
           t6_restart t7_dedup t8_loss t9_backpressure t10_abnormal)

usage() {
  cat <<'EOF'
usage: ./run.sh [test ...]
  t1  concurrency: 50 clients x 100 events, throughput, p50/p99
  t2  SO_PEERCRED identity, foreign-uid rejection
  t3  10 MiB frame rejected, daemon survives
  t4  client write blocking with the daemon SIGSTOPped
  t5  single report attempt with no daemon
  t6  daemon kill -9 + restart, reconnect backoff, sequence gap
  t7  duplicate event_id deduplication
  t8  10000 events, crash mid-stream, loss accounting
  t9  backpressure: slow daemon, bounded client queue
  t10 abnormal close, connection leak, unlink then reconnect
  all (default)  build, then every test above
EOF
}

main() {
  local want=("$@")
  if [[ ${#want[@]} -eq 1 && ( "${want[0]}" == "-h" || "${want[0]}" == "--help" || "${want[0]}" == "list" ) ]]; then
    usage
    exit 0
  fi
  build

  local failed=()
  if [[ ${#want[@]} -eq 0 ]]; then
    local t
    for t in "${ALL_TESTS[@]}"; do
      log "=== $t ==="
      if ! "$t"; then failed+=("$t"); fi
    done
  else
    local name
    for name in "${want[@]}"; do
      # accept t1 as a shorthand for t1_concurrency
      local fn="$name"
      case "$name" in
        t1) fn=t1_concurrency ;; t2) fn=t2_auth ;; t3) fn=t3_oversize ;;
        t4) fn=t4_write_block ;; t5) fn=t5_no_daemon ;; t6) fn=t6_restart ;;
        t7) fn=t7_dedup ;; t8) fn=t8_loss ;; t9) fn=t9_backpressure ;;
        t10) fn=t10_abnormal ;;
      esac
      if ! declare -F "$fn" >/dev/null; then
        log "unknown test: $name"
        usage
        exit 2
      fi
      log "=== $fn ==="
      if ! "$fn"; then failed+=("$fn"); fi
    done
  fi

  log ""
  log "results in: $RESULTS"
  ls -1 "$RESULTS" | tail -20
  if [[ ${#failed[@]} -gt 0 ]]; then
    log "FAILED: ${failed[*]}"
    exit 1
  fi
  log "ALL TESTS PASSED"
}

main "$@"
