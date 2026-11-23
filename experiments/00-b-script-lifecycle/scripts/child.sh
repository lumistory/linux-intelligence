#!/bin/bash
# Called by nested.sh; spawns a grandchild to make the tree three deep.
echo "child:pid=$$ ppid=$PPID"
sleep 0.10
bash "$(dirname "$0")/grandchild.sh"
echo "child:done"
