#!/bin/bash
# Case: N short-lived external commands, each living for a controlled duration.
# Used to measure P(detected) as a function of process lifetime for a /proc
# sampler with a fixed period.
#
#   lifetime.sh <repeat> <seconds-per-command>
REPEAT="${1:-50}"
DWELL="${2:-0.010}"
for _ in $(seq 1 "$REPEAT"); do
    /usr/bin/sleep "$DWELL"
done
echo "lifetime:repeat=$REPEAT dwell=$DWELL"
