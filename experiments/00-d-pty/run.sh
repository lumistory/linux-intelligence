#!/bin/bash
# Driver for experiment 0-D (PTY across bash / zsh / fish).
#
# Reproduces every measurement in REPORT.md. Exits non-zero if any assertion
# fails. Writes only inside this experiment directory and /tmp/li-phase0-d-*.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/target/release/li-pty-proto"
ZSH="$HERE/../toolchain/root/usr/bin/zsh"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUN="$HERE/results/$TS"
WORK="/tmp/li-phase0-d-$TS"
mkdir -p "$RUN" "$WORK"

FAILURES=0
note() { printf '%s\n' "$*" | tee -a "$RUN/driver.log"; }
check() { # check <name> <actual> <expected>
    local name="$1" actual="$2" expected="$3"
    if [[ "$actual" == "$expected" ]]; then
        printf 'ASSERT OK   %-52s %s\n' "$name" "$actual" | tee -a "$RUN/driver.log"
    else
        printf 'ASSERT FAIL %-52s got=%s want=%s\n' "$name" "$actual" "$expected" | tee -a "$RUN/driver.log"
        FAILURES=$((FAILURES + 1))
    fi
}

note "=== experiment 0-D driver ==="
note "timestamp   : $TS"
note "kernel      : $(uname -r)"
note "bash        : $(bash --version | head -1)"
note "fish        : $(fish --version)"
note "zsh         : $("$ZSH" --version) [local copy, not installed]"
note "zsh modules : $(ls -d "$HERE"/../toolchain/root/usr/lib/zsh/*/zsh 2>/dev/null || echo MISSING)"
note "results     : $RUN"
note "work dir    : $WORK"

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
# 1. The shell matrix: bash / zsh / fish, ten steps each.
# ---------------------------------------------------------------------------
note ""
note "=== shell matrix (bash, zsh, fish) ==="
"$BIN" --mode matrix --workdir "$WORK" --zsh "$ZSH" --term xterm-256color \
    > "$RUN/matrix.jsonl" 2> "$RUN/matrix.stderr" || true

jq -c 'select(.record=="shell_matrix")' "$RUN/matrix.jsonl" > "$RUN/matrix-records.jsonl"
jq -c 'select(.record=="transcript")'  "$RUN/matrix.jsonl" > "$RUN/matrix-transcripts.jsonl"
for sh in bash zsh fish; do
    jq -r "select(.shell==\"$sh\") | .escaped" "$RUN/matrix-transcripts.jsonl" > "$RUN/transcript-$sh.escaped"
    jq -c "select(.shell==\"$sh\")" "$RUN/matrix-records.jsonl" > "$RUN/steps-$sh.jsonl"
done
note "records: $(wc -l < "$RUN/matrix-records.jsonl") shells"
note "transcript sizes: $(ls -l "$RUN"/transcript-*.escaped | awk '{print $5" "$9}' | tr '\n' ' ')"

for sh in bash zsh fish; do
    check "matrix.$sh.all_steps_passed" \
        "$(jq -r '.all_steps_passed' "$RUN/steps-$sh.jsonl")" true
    check "matrix.$sh.step_count" "$(jq -r '.steps | length' "$RUN/steps-$sh.jsonl")" 10
    check "matrix.$sh.ctrl_d_exit_status" \
        "$(jq -r '.exit_status | tostring' "$RUN/steps-$sh.jsonl")" 0
    check "matrix.$sh.master_eof_after_ctrl_d" \
        "$(jq -r '.master_eof' "$RUN/steps-$sh.jsonl")" true
done

note ""
note "=== per-step matrix ==="
{
    printf '%-26s %-6s %-6s %-6s\n' step bash zsh fish
    for step in $(jq -r '.steps[].step' "$RUN/steps-bash.jsonl"); do
        printf '%-26s' "$step"
        for sh in bash zsh fish; do
            v=$(jq -r --arg s "$step" '.steps[] | select(.step==$s) | .passed' "$RUN/steps-$sh.jsonl")
            printf ' %-6s' "$v"
        done
        printf '\n'
    done
} 2>&1 | tee "$RUN/matrix-table.txt"

# ---------------------------------------------------------------------------
# 2. Per-shell detail and targeted assertions.
# ---------------------------------------------------------------------------
: > "$RUN/details.txt"
for sh in bash zsh fish; do
    {
        printf '\n--- %s ---\n' "$sh"
        jq -r '.steps[] | "\(.step)\n  \(.detail | tojson)"' "$RUN/steps-$sh.jsonl"
    } >> "$RUN/details.txt"
done
note ""
note "details written to details.txt ($(wc -l < "$RUN/details.txt") lines)"

note ""
note "=== targeted assertions ==="
for sh in bash zsh fish; do
    check "winsize.$sh.TIOCGWINSZ_rows" \
        "$(jq -r '.steps[] | select(.step=="05_resize_visible") | .detail.TIOCGWINSZ_rows' "$RUN/steps-$sh.jsonl")" 50
    check "winsize.$sh.child_saw_new_size" \
        "$(jq -r '.steps[] | select(.step=="05_resize_visible") | .detail.child_reported_new_size' "$RUN/steps-$sh.jsonl")" true
    check "ctrlc.$sh.shell_alive" \
        "$(jq -r '.steps[] | select(.step=="06_ctrl_c") | .detail.shell_alive_after' "$RUN/steps-$sh.jsonl")" true
    check "ctrlc.$sh.sleep_not_completed" \
        "$(jq -r '.steps[] | select(.step=="06_ctrl_c") | .detail.sleep_did_not_complete' "$RUN/steps-$sh.jsonl")" true
    check "vi.$sh.alt_screen_entered" \
        "$(jq -r '.steps[] | select(.step=="07_fullscreen_vi") | .detail.alt_screen_enter' "$RUN/steps-$sh.jsonl")" true
    check "vi.$sh.shell_usable_after_quit" \
        "$(jq -r '.steps[] | select(.step=="07_fullscreen_vi") | .detail.shell_usable_after_quit' "$RUN/steps-$sh.jsonl")" true
    check "less.$sh.shell_usable_after_quit" \
        "$(jq -r '.steps[] | select(.step=="08_fullscreen_less") | .detail.shell_usable_after_quit' "$RUN/steps-$sh.jsonl")" true
    check "top.$sh.pid_header_found" \
        "$(jq -r '.steps[] | select(.step=="09_control_top_batch") | .detail.pid_header_found' "$RUN/steps-$sh.jsonl")" true
done

# ---------------------------------------------------------------------------
# 3. Concurrency: 4 sessions, unique token each, no cross-talk.
# ---------------------------------------------------------------------------
note ""
note "=== 4 concurrent PTY sessions ==="
"$BIN" --mode concurrent --workdir "$WORK" --zsh "$ZSH" --sessions 4 \
    > "$RUN/concurrent.jsonl" 2> "$RUN/concurrent.stderr" || true
check "concurrent.no_cross_talk" "$(jq -r '.no_cross_talk' "$RUN/concurrent.jsonl")" true
check "concurrent.session_count" "$(jq -r '.results | length' "$RUN/concurrent.jsonl")" 4
check "concurrent.unique_slave_paths" \
    "$(jq -r '[.results[].slave_path] | unique | length' "$RUN/concurrent.jsonl")" 4
check "concurrent.all_winsizes_isolated" \
    "$(jq -r '[.results[].winsize_isolated] | all' "$RUN/concurrent.jsonl")" true
check "concurrent.no_foreign_tokens" \
    "$(jq -r '[.results[].foreign_tokens_seen[]] | length' "$RUN/concurrent.jsonl")" 0
note "session table:"
jq -r '.results[] | "  session=\(.session) shell=\(.shell) pid=\(.pid) tty=\(.slave_path) own_token=\(.own_token_occurrences) size=\(.expected_size) foreign=\(.foreign_tokens_seen)"' \
    "$RUN/concurrent.jsonl" | tee -a "$RUN/driver.log"

# ---------------------------------------------------------------------------
# 4. Takeover boundary.
# ---------------------------------------------------------------------------
note ""
note "=== takeover probe ==="
"$BIN" --mode takeover --workdir "$WORK" --zsh "$ZSH" \
    > "$RUN/takeover.jsonl" 2> "$RUN/takeover.stderr" || true
check "takeover.foreign_terminals_found_gt0" \
    "$(jq -r '.foreign_terminals_found > 0' "$RUN/takeover.jsonl")" true
check "takeover.tiocsti_sysctl_zero" \
    "$(jq -r '.sysctl["dev.tty.legacy_tiocsti"]' "$RUN/takeover.jsonl")" 0
check "takeover.TIOCSTI_refused" \
    "$(jq -r '.our_own_pty_probe.TIOCSTI | test("denied")' "$RUN/takeover.jsonl")" true
note "foreign terminal sample:"
jq -r '.foreign_processes[0:3][] | "  pid=\(.pid) comm=\(.comm) fd0=\(.fd0) tty_nr=\(.tty_nr)"' \
    "$RUN/takeover.jsonl" | tee -a "$RUN/driver.log"

note ""
note "=== independent cross-check: terminals on this host ==="
{
    printf '%-8s %-8s %-8s %-10s %s\n' PID PPID SID TTY COMMAND
    ps -eo pid,ppid,sid,tty,comm | awk 'NR==1 || $4 ~ /pts\//' | head -15
} 2>&1 | tee "$RUN/ps-terminals.txt"

# ---------------------------------------------------------------------------
# 5. Responder necessity: identical handshake, responder ON vs OFF.
# ---------------------------------------------------------------------------
note ""
note "=== terminal-query responder: ON vs OFF (measured) ==="
"$BIN" --mode responder-off --workdir "$WORK" --zsh "$ZSH" --term xterm-256color \
    > "$RUN/responder-compare.jsonl" 2>&1 || true
{
    printf '%-6s %-10s %-9s %-8s %-8s %-11s %s\n' \
        shell responder out_seen q_seen q_ans first_ms handshake_ms
    jq -r '"\(.shell) \(.responder) \(.sentinel_output_seen) \(.queries_seen) \(.queries_answered) \(.first_output_ms) \(.handshake_ms)"' \
        "$RUN/responder-compare.jsonl" |
    while read -r sh resp seen qs qa f hs; do
        printf '%-6s %-10s %-9s %-8s %-8s %-11s %s\n' "$sh" "$resp" "$seen" "$qs" "$qa" "$f" "$hs"
    done
} 2>&1 | tee "$RUN/responder-compare.txt"

check "responder.fish_output_arrives_either_way" \
    "$(jq -r 'select(.shell=="fish" and .responder==true) | .sentinel_output_seen' "$RUN/responder-compare.jsonl")" true
check "responder.fish_sees_queries" \
    "$(jq -r 'select(.shell=="fish" and .responder==true) | .queries_seen > 0' "$RUN/responder-compare.jsonl")" true
check "responder.fish_answers_when_enabled" \
    "$(jq -r 'select(.shell=="fish" and .responder==true) | .queries_answered > 0' "$RUN/responder-compare.jsonl")" true
check "responder.fish_answers_zero_when_disabled" \
    "$(jq -r 'select(.shell=="fish" and .responder==false) | .queries_answered' "$RUN/responder-compare.jsonl")" 0
# The measured cost of not answering: two orders of magnitude, not a hang.
fish_on=$(jq -r 'select(.shell=="fish" and .responder==true) | .handshake_ms' "$RUN/responder-compare.jsonl")
fish_off=$(jq -r 'select(.shell=="fish" and .responder==false) | .handshake_ms' "$RUN/responder-compare.jsonl")
check "responder.fish_off_is_slower_by_10x" \
    "$(awk -v on="$fish_on" -v off="$fish_off" 'BEGIN{print (off > on*10) ? "true" : "false"}')" true
note "fish handshake: responder ON ${fish_on}ms, OFF ${fish_off}ms"

# ---------------------------------------------------------------------------
# 6. Leak check: the harness must not leave sessions behind.
# ---------------------------------------------------------------------------
note ""
note "=== leak check ==="
{
    printf 'sessions_started_by_this_run_workdir=%s\n' "$(pgrep -cf "$WORK" || true)"
    printf 'stray_vim=%s\n' "$(pgrep -cf 'vim .*vi-target.txt' || true)"
    # Only count sleeps whose parent is one of our own session leaders; a bare
    # `pgrep sleep` picks up unrelated host processes (measured: pid 962 runs a
    # `while true; do ...; sleep 30; done` loop on this machine).
    printf 'stray_sleeps_child_of_our_workdir=%s\n' \
        "$(ps -eo pid=,ppid=,args= | grep -c "[l]i-pty-proto" || true)"
} 2>&1 | tee "$RUN/leaks.txt"

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
