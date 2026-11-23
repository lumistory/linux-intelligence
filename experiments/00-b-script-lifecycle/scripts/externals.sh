#!/bin/bash
# Case: N short-lived external commands, each with a distinguishable argv.
# Used to measure how many forks a /proc sampler actually catches.
N="${1:-200}"
for i in $(seq 1 "$N"); do
    /bin/echo "EXT-$i" >/dev/null
done
echo "externals:submitted=$N"
