#!/bin/bash
# Case: a background child that outlives the script itself.
sleep 1.5 &
bg_pid=$!
echo "bg:launched pid=$bg_pid"
echo "bg:script-exiting-now"
exit 0
