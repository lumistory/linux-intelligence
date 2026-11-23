#!/bin/bash
# Case: exec replaces the shell image; pid must not change.
#
# The 200 ms dwell is deliberate: `exec` on a process that lives for only a
# millisecond is invisible to a /proc sampler (see execrepl-fast.sh for the
# negative case).
echo "execrepl:about-to-exec pid=$$"
sleep 0.2
exec /usr/bin/sleep 0.3
