#!/usr/bin/env bash
# run.sh -- reproduce experiment 00-a from scratch.
#
#   ./run.sh              full run (about 4 minutes)
#   ./run.sh --quick      skip the 100-iteration latency runs
#   ./run.sh --cases      only the 13 x 3 interaction matrix
#
# Exits non-zero if anything that must be silent spoke, if a case produced no
# events, or if the detector self-test fails.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
RES="$HERE/results"
HARNESS="$HERE/harness"
ADAPT="$HERE/adapters"
ZSH="$HERE/../toolchain/root/usr/bin/zsh"
PY=${PYTHON:-python3}
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
LOG="$RES/run-$STAMP.log"

mkdir -p "$RES" "$RES/latency" /tmp/li-phase0-a/rc
: > "$LOG"

say() { printf '%s\n' "$*" | tee -a "$LOG" ; }
step() { say ""; say "=== $* ===" ; }

[ -x "$ZSH" ] || { say "FATAL: no zsh at $ZSH"; exit 1; }
say "run started $STAMP"
say "bash=$(bash --version | head -1)"
say "zsh=$("$ZSH" --version)"
say "fish=$(fish --version)"
say "python=$($PY -V 2>&1)"

# --- 0. isolated shell rc files (never touch ~) --------------------------
P="⟦LI-P⟧"
printf 'PS1=%q\nPS2=%q\n' "$P " "⟦LI-P2⟧ " > /tmp/li-phase0-a/rc/bashrc
say "wrote isolated bash rcfile /tmp/li-phase0-a/rc/bashrc"

# --- 1. build the emit helper -------------------------------------------
step "build harness/li_emit"
gcc -O2 -Wall -Wextra -std=c11 -o "$HARNESS/li_emit" "$HARNESS/li_emit.c" 2>&1 | tee -a "$LOG"
[ -x "$HARNESS/li_emit" ] || { say "FATAL: li_emit did not build"; exit 1; }

# --- 2. syntax check -----------------------------------------------------
step "syntax check"
bash -n "$ADAPT/bash.sh"
"$ZSH" -n "$ADAPT/zsh.zsh"
fish -n "$ADAPT/fish.fish"
bash -n "$HARNESS/noninteractive.sh"
for f in "$HARNESS"/*.py; do "$PY" -c "import ast,sys;ast.parse(open(sys.argv[1]).read())" "$f"; done
say "all adapters and harness scripts parse"

# --- 3. mechanism probes -------------------------------------------------
step "mechanism probes"
"$PY" "$HARNESS/probes.py" 2>&1 | tee -a "$LOG"

# --- 4. the 13 x 3 interaction matrix ------------------------------------
step "interaction cases (13 x 3)"
"$PY" "$HARNESS/cases.py" 2>&1 | tee -a "$LOG"
"$PY" "$HARNESS/analyze.py" 2>&1 | tee -a "$LOG"

# --- 5. negative control: non-interactive must be silent ------------------
step "non-interactive control"
bash "$HARNESS/noninteractive.sh"
cat "$RES/noninteractive.log" | tee -a "$LOG"
if grep -q HOOK_FIRED "$RES/noninteractive.log"; then
  say "FAIL: a hook fired in a non-interactive shell"
  exit 1
fi
say "OK: no hook fired outside an interactive shell"

# --- 6. latency ----------------------------------------------------------
if [ "${1:-}" = "--cases" ]; then
  say "skipping latency (--cases)"
else
  step "prompt detector self-test"
  "$PY" "$HARNESS/latency.py" --selftest 2>&1 | tee -a "$LOG"
  if [ "${1:-}" = "--quick" ]; then
    step "latency (quick, 20 iterations)"
    "$PY" "$HARNESS/latency.py" --iters 20 \
      --modes empty,devnull,deadsock,live 2>&1 | tee -a "$LOG"
  else
    step "latency (100 iterations)"
    "$PY" "$HARNESS/latency.py" --iters 100 \
      --modes empty,devnull,deadsock,live \
      --out "$RES/latency/latency.json" 2>&1 | tee -a "$LOG"
  fi
fi

# --- 7. sanity: every case produced events -------------------------------
step "sanity checks"
"$PY" - "$RES" << 'PYEOF' 2>&1 | tee -a "$LOG"
import json, os, sys
res = sys.argv[1]
analysis = json.load(open(os.path.join(res, "analysis.json")))
bad = [k for k, v in analysis.items()
       if v.get("status") == "NOT TESTED" or not v.get("n_events")]
if bad:
    print("FAIL: cases with no recorded events:", bad)
    sys.exit(1)
print("OK: %d case/shell combinations recorded events" % len(analysis))
lat = json.load(open(os.path.join(res, "latency", "latency.json")))
for k in sorted(lat):
    v = lat[k]
    if "error" in v:
        print("WARN: latency %s failed: %s" % (k, v["error"]))
        continue
    print("latency %-14s median=%6.0f p99=%6s us"
          % (k, v["median_us"], v["p99_us"]))
PYEOF

say ""
say "run finished; log: $LOG"
