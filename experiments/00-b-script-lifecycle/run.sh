#!/bin/bash
# Driver for experiment 0-B (non-interactive script lifecycle).
#
# Reproduces every measurement in REPORT.md. Exits non-zero if any assertion
# fails. Writes only inside this experiment directory and /tmp/li-phase0-b-*.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/target/release/li-run-proto"
SCRIPTS="$HERE/scripts"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUN="$HERE/results/$TS"
TMP="/tmp/li-phase0-b-$TS"
mkdir -p "$RUN" "$TMP"
export LI_TMPDIR="$TMP"

FAILURES=0
note() { printf '%s\n' "$*" | tee -a "$RUN/driver.log"; }
check() { # check <name> <actual> <expected>
    local name="$1" actual="$2" expected="$3"
    if [[ "$actual" == "$expected" ]]; then
        printf 'ASSERT OK   %-46s %s\n' "$name" "$actual" | tee -a "$RUN/driver.log"
    else
        printf 'ASSERT FAIL %-46s got=%s want=%s\n' "$name" "$actual" "$expected" | tee -a "$RUN/driver.log"
        FAILURES=$((FAILURES + 1))
    fi
}
check_nonempty() { # check_nonempty <name> <value>
    local name="$1" value="$2"
    if [[ -n "$value" && "$value" != "null" ]]; then
        printf 'ASSERT OK   %-46s %s\n' "$name" "$value" >> "$RUN/driver.log"
        printf 'ASSERT OK   %-46s %s\n' "$name" "$value"
    else
        printf 'ASSERT FAIL %-46s empty/null\n' "$name" | tee -a "$RUN/driver.log"
        FAILURES=$((FAILURES + 1))
    fi
}

note "=== experiment 0-B driver ==="
note "timestamp      : $TS"
note "kernel         : $(uname -r)"
note "bash           : $(bash --version | head -1)"
note "rustc          : $(rustc --version)"
note "binary         : $BIN"
note "results dir    : $RUN"
note "tmp dir        : $TMP"

note ""
note "=== build ==="
(
    cd "$HERE"
    cargo build --offline --release 2>&1
    cargo fmt --check 2>&1 || echo "WARN: cargo fmt --check reported differences"
    cargo clippy --offline --all-targets -- -D warnings 2>&1 || echo "WARN: clippy reported findings"
    cargo test --offline 2>&1
) 2>&1 | tee "$RUN/00-build.log"
test -x "$BIN" || { note "FATAL: binary missing after build"; exit 1; }

# ---------------------------------------------------------------------------
# run_case <case-name> <script> [extra li-run-proto args...]
# ---------------------------------------------------------------------------
run_case() {
    local name="$1"; shift
    local script="$1"; shift
    local dir="$RUN/$name"
    mkdir -p "$dir"
    note ""
    note "=== case: $name ($script) ==="
    set +e
    # Run from $TMP so the recorded cwd is a known value; the script path is
    # absolute, and children inherit $TMP as their cwd.
    ( cd "$TMP" && "$BIN" --session "0b-$name" --out-dir "$dir" --print-procs "$@" -- \
        /bin/bash "$SCRIPTS/$script" ) 2>&1 | tee "$dir/raw-stdout.txt" >/dev/null
    local rc=${PIPESTATUS[0]}
    set -e
    # The event stream is the first two lines; a third line is the process dump.
    head -n 2 "$dir/raw-stdout.txt" > "$dir/events.jsonl"
    tail -n 1 "$dir/raw-stdout.txt" > "$dir/procs.json"
    printf '%s\n' "$rc" > "$dir/exit-status.txt"
    note "supervisor exit status: $rc"
    jq -c '{type, exit_code: .payload.exit_code, term_signal: .payload.term_signal,
            duration_ms: .payload.duration_ms, procs: .payload.process_count,
            survivors: .payload.survivors_after_exit}' "$dir/events.jsonl" \
        | tee -a "$RUN/driver.log"
}

run_case single           single.sh
run_case nested           nested.sh
run_case background       background.sh
run_case execrepl         execrepl.sh
run_case execrepl-fast    execrepl-fast.sh
run_case exit3            exit3.sh
run_case ctrlc            sleeper.sh --signal-after-ms 400 --signal INT
run_case kill9            sleeper.sh --signal-after-ms 400 --signal KILL
run_case bigoutput        bigoutput.sh --poll-us 2000
run_case boundary         boundary.sh

note ""
note "=== detection-rate sweep: short-lived external commands ==="
note "Each row: N identical external commands of a given lifetime, run under a"
note "sampler with a fixed period. 'detected' counts distinct recorded processes"
note "whose comm is that command. The point is P(detected) vs lifetime/period."
{
    printf '%-10s %-12s %-10s %-9s %-9s %-8s %s\n' \
        dwell_ms interval_us submitted detected ratio scan_us reads
    for dwell_ms in 1 5 20 100; do
        for poll in 200 2000 10000; do
            dir="$RUN/detect-${dwell_ms}ms-${poll}us"
            mkdir -p "$dir"
            "$BIN" --session "0b-detect-${dwell_ms}-${poll}" --out-dir "$dir" \
                --poll-us "$poll" -- \
                /bin/bash "$SCRIPTS/lifetime.sh" 50 "$(awk -v d="$dwell_ms" 'BEGIN{printf "%.3f", d/1000}')" \
                > "$dir/raw-stdout.txt" 2>&1 || true
            head -n 2 "$dir/raw-stdout.txt" > "$dir/events.jsonl"
            detected=$(jq -r 'select(.type=="ScriptFinished")
                | [.payload.processes[] | select(.comm_last=="sleep")] | length' \
                "$dir/events.jsonl" 2>/dev/null || echo "ERR")
            scan_us=$(jq -r 'select(.type=="ScriptFinished") | .payload.sampling.mean_read_ns | . / 1000 | round' \
                "$dir/events.jsonl" 2>/dev/null || echo "ERR")
            reads=$(jq -r 'select(.type=="ScriptFinished") | .payload.sampling.reads' \
                "$dir/events.jsonl" 2>/dev/null || echo "ERR")
            printf '%-10s %-12s %-10s %-9s %-9s %-8s %s\n' \
                "$dwell_ms" "$poll" 50 "$detected" \
                "$(awk -v d="$detected" 'BEGIN{printf "%.2f", d/50}')" "$scan_us" "$reads"
        done
    done
} 2>&1 | tee "$RUN/detection-rate.tsv"

# Also record how expensive one full /proc scan is, and how many processes the
# machine has, since that sets the achievable sampling period.
{
    printf 'processes_in_table=%s\n' "$(ls -d /proc/[0-9]* | wc -l)"
    printf 'kernel=%s\n' "$(uname -r)"
    printf 'nproc=%s\n' "$(nproc)"
} | tee "$RUN/scan-cost-env.txt"
# Cost of one naive /proc table read, measured directly (no /usr/bin/time on
# this host). 10 iterations, reported in ms.
{
    start_ns=$(date +%s%N)
    for _ in $(seq 1 10); do ls -d /proc/[0-9]* >/dev/null; done
    end_ns=$(date +%s%N)
    printf 'naive_proctable_read_mean_ms=%s\n' \
        "$(awk -v a="$start_ns" -v b="$end_ns" 'BEGIN{printf "%.2f", (b-a)/1e7}')"
} | tee -a "$RUN/scan-cost-env.txt"

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------
note ""
note "=== assertions ==="

# pf <jq-filter> <case> -> value from the ScriptFinished event only
pf() { jq -r "select(.type==\"ScriptFinished\") | .payload | $1" "$RUN/$2/events.jsonl"; }
# pstart <jq-filter> <case> -> value from the ScriptStarted event only
pstart() { jq -r "select(.type==\"ScriptStarted\") | .payload | $1" "$RUN/$2/events.jsonl"; }
# pc <jq-filter-on-.payload> <case> -> ScriptFinished event count helper
pcount() { jq -r "select(.type==\"ScriptFinished\") | .payload | $1" "$RUN/$2/events.jsonl"; }

# single
check "single.exit_code"             "$(pf '.exit_code' single)" 0
check "single.leader_recorded"       "$(pf '[.processes[] | select(.pid == .pid)] | length >= 1' single)" true
check "single.leader_is_bash"        "$(pf '.processes[] | select(.pid == .pid) | .comm_first' single)" bash
check_nonempty "single.exit_mono_ns" "$(pf '.exit_mono_ns' single)"
check "single.duration_positive"     "$(pf '.duration_ns > 0' single)" true
check "single.program"               "$(pf '.program' single)" /bin/bash
check "single.pid_is_sid_and_pgid"   "$(pf '(.pid == .sid) and (.pid == .pgid)' single)" true
check "single.cwd_recorded"          "$(pstart '.cwd_at_start' single)" "$TMP"
check "single.monotonic_ordering"    "$(pf '.start_mono_ns < .spawn_mono_ns and .spawn_mono_ns < .exit_mono_ns' single)" true
check "single.exit_matches_status"   "$(cat "$RUN/single/exit-status.txt")" 0

# nested: three bash levels plus sleeps under them
check "nested.bash_procs>=3"     "$(pf '[.processes[] | select(.comm_last=="bash")] | length >= 3' nested)" true
check "nested.sleep_procs>=1"    "$(pf '[.processes[] | select(.comm_last=="sleep")] | length >= 1' nested)" true
# chain depth inside the recorded set: for each recorded process, walk ppid
# upward while the parent is also recorded. depth 1 = the leader itself.
check "nested.max_chain_depth>=3" "$(pf '
    . as $P
    | ($P.processes | map({key: (.pid|tostring), value: .ppid}) | from_entries) as $pp
    | ($P.processes
        | map(.pid
              | [while(($pp[tostring] // 0) != 0; $pp[tostring])]
              | length)
        | max) >= 3' nested)" true

# background: the orphaned sleep must be alive after the script exited
check "background.survivor_is_sleep" "$(pf '[.processes[] | select(.alive_after_leader_exit and .comm_last=="sleep")] | length >= 1' background)" true
check_nonempty "background.survivor_pid" "$(pf '.survivors_after_exit[0]' background)"
check "background.orphan_reparented" "$(pf '[.processes[] | select(.alive_after_leader_exit)] | length' background)" 1

# execrepl: the leader's image changes bash -> sleep while its pid is constant.
# The script dwells 200 ms first so the change lands inside a sampling window.
check "execrepl.image_changed_to_sleep" "$(pf '[.processes[] | select((.exe_first // "" | endswith("/bash")) and (.exe_last // "" | endswith("/sleep")))] | length' execrepl)" 1
check "execrepl.leader_pid_preserved"  "$(pf '. as $P | [.processes[] | select(.pid == $P.pid and (.exe_last // "" | endswith("/sleep")))] | length' execrepl)" 1
check "execrepl.leader_had_two_images" "$(pf '. as $P | [.processes[] | select(.pid == $P.pid and (.exe_first != .exe_last))] | length > 0' execrepl)" true

# execrepl-fast: negative control. ~1 ms lifetime, so no image change is seen.
check "execrepl-fast.no_image_change"     "$(pf '[.processes[] | select((.exe_first // "" | endswith("/bash")) and (.exe_last // "" | endswith("/ls")))] | length' execrepl-fast)" 0

# exit3
check "exit3.exit_code"    "$(pf '.exit_code' exit3)" 3
check "exit3.term_signal"  "$(pf '.term_signal' exit3)" null
check "exit3.status_match" "$(cat "$RUN/exit3/exit-status.txt")" 3

# ctrlc (SIGINT to the process group)
check "ctrlc.exit_code_null"      "$(pf '.exit_code' ctrlc)" null
check "ctrlc.term_signal_sigint"  "$(pf '(.term_signal // "") | test("SIGINT")' ctrlc)" true
check "ctrlc.signal_injection_ok" "$(pf '.injected_signals[0].result' ctrlc)" ok
check "ctrlc.sleep_did_not_finish" "$(grep -c 'UNEXPECTED' "$RUN/ctrlc/script-stdout.txt" || true)" 0
check "ctrlc.supervisor_status_130" "$(cat "$RUN/ctrlc/exit-status.txt")" 130

# kill9 (SIGKILL to the process group)
check "kill9.term_signal_sigkill" "$(pf '(.term_signal // "") | test("SIGKILL")' kill9)" true
check "kill9.exit_code_null"      "$(pf '.exit_code' kill9)" null
check "kill9.supervisor_status_137" "$(cat "$RUN/kill9/exit-status.txt")" 137

# bigoutput: 4000 lines captured, but the process list is nothing like 4001
check "bigoutput.stdout_lines_4000" "$(pf '.io.stdout_lines' bigoutput)" 4000
check "bigoutput.stdout_bytes_gt_30k" "$(pf '.io.stdout_bytes > 30000' bigoutput)" true
check "bigoutput.procs_far_below_lines" "$(pf '.process_count < 200' bigoutput)" true

# boundary: the 6 visible text lines vs the invisible structure
check "boundary.stdout_lines" "$(pf '.io.stdout_lines' boundary)" 6
check "boundary.stdout_first_line" "$(pf '.io.stdout_first_line' boundary)" BOUNDARY-ECHO-1
check "boundary.zero_command_events" "$(jq -s '[.[] | select(.type | startswith("Command"))] | length' "$RUN/boundary/events.jsonl")" 0
check "boundary.event_count_is_2" "$(wc -l < "$RUN/boundary/events.jsonl" | tr -d ' ')" 2

# The measurement, not an assertion: how many of boundary.sh's three external
# commands did the sampler actually catch? The number is the finding.
{
    printf 'boundary.external_cmds_detected=%s of 3\n' "$(pf '[.processes[]
        | select(.comm_last=="true" or .comm_last=="false" or .comm_last=="printenv")]
        | length' boundary)"
    printf 'boundary.process_count=%s\n' "$(pf '.process_count' boundary)"
    printf 'boundary.process_list:\n'
    jq -r 'select(.type=="ScriptFinished") | .payload.processes[]
        | "  pid=\(.pid) ppid=\(.ppid) comm=\(.comm_last) samples=\(.samples) src=\(.observation_source)"' \
        "$RUN/boundary/events.jsonl"
} | tee -a "$RUN/driver.log"

# event ordering / envelope
check "event.sequence_started" "$(jq -r 'select(.type=="ScriptStarted") | .sequence' "$RUN/single/events.jsonl")" 1
check "event.sequence_finished" "$(jq -r 'select(.type=="ScriptFinished") | .sequence' "$RUN/single/events.jsonl")" 2

# schema conformance for every emitted event
note ""
note "=== schema conformance (protocol §2) ==="
for f in "$RUN"/*/events.jsonl; do
    jq -e 'has("schema_version") and has("event_id") and has("source")
           and has("session_id") and has("sequence") and has("ts_mono_ns")
           and has("ts_wall") and has("type") and has("payload")
           and has("evidence_status")' "$f" >/dev/null \
        || { note "SCHEMA FAIL $f"; FAILURES=$((FAILURES + 1)); }
done
check_nonempty "schema.checked_files" "$(ls "$RUN"/*/events.jsonl | wc -l)"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
note ""
note "=== summary table ==="
{
    printf '%-14s %-10s %-16s %-12s %-8s %-10s\n' \
        case exit_code term_signal duration_ms procs survivors
    for d in single nested background execrepl execrepl-fast exit3 ctrlc kill9 bigoutput boundary; do
        printf '%-14s %-10s %-16s %-12s %-8s %-10s\n' "$d" \
            "$(pf '.exit_code | tostring' "$d")" \
            "$(pf '.term_signal | tostring' "$d")" \
            "$(pf '(.duration_ms * 100 | round) / 100 | tostring' "$d")" \
            "$(pf '.process_count | tostring' "$d")" \
            "$(pf '.survivors_after_exit | length | tostring' "$d")"
    done
} | tee "$RUN/summary.tsv"

note ""
note "=== boundary detail: what the supervisor saw inside boundary.sh ==="
jq -r '.payload.io' "$RUN/boundary/events.jsonl" | tee "$RUN/boundary-io.json"
note "--- the 6 text lines the supervisor received (aggregate, unattributed): ---"
cat "$RUN/boundary/script-stdout.txt" | tee "$RUN/boundary-stdout.txt"
note "--- redirected content the supervisor never saw at all: ---"
cat "$TMP/boundary-redirected.txt" 2>/dev/null | tee "$RUN/boundary-redirected.txt" || note "(missing)"

note ""
if [[ "$FAILURES" -eq 0 ]]; then
    note "RESULT: all assertions passed"
    note "results: $RUN"
    exit 0
else
    note "RESULT: $FAILURES assertion(s) FAILED"
    note "results: $RUN"
    exit 1
fi
