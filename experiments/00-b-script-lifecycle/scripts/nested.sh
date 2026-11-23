#!/bin/bash
# Case: script calls script calls script.
echo "nested:pid=$$ ppid=$PPID"
bash "$(dirname "$0")/child.sh"
echo "nested:done"
