#!/bin/bash
# Case: the observability boundary, made explicit.
#
# Five visible echo lines, plus silent work that leaves no process trace at
# all (builtins, a redirect, a variable), plus exactly three short-lived
# external commands whose lifetime is ~1ms.

echo "BOUNDARY-ECHO-1"
echo "BOUNDARY-ECHO-2"
echo "BOUNDARY-ECHO-3"
echo "BOUNDARY-ECHO-4"
echo "BOUNDARY-ECHO-5"

# Invisible to any process observer: builtins, no fork, no exec.
: $((1 + 1))
x="secret-value"
cd /tmp
cd - >/dev/null
umask 022
unset x

# Invisible content: a redirect writes data that never reaches the supervisor
# as a separable event; it only shows up as more bytes in the aggregate file.
echo "BOUNDARY-REDIRECTED" > "$LI_TMPDIR/boundary-redirected.txt"

# Short-lived external commands: real forks, ~1ms each. Whether they are seen
# depends entirely on the sampling window.
/bin/true
/bin/false || true
/usr/bin/printenv LI_SESSION_ID >/dev/null

echo "BOUNDARY-ECHO-6"
