#!/bin/sh
# 0-F step 2: /bin/sh implementation differences, proven by execution.
set -u

echo "--- bash version"
bash --version 2>&1 | head -n 1

echo "--- /bin/sh resolves to"
readlink -f /bin/sh 2>&1

echo "--- shell binaries present"
for s in sh bash dash zsh fish busybox ksh mksh yash; do
    if command -v "$s" >/dev/null 2>&1; then
        printf '%s: %s\n' "$s" "$(command -v "$s")"
    else
        printf '%s: ABSENT\n' "$s"
    fi
done

echo "--- TEST A: is BASH_VERSION set under /bin/sh ?"
/bin/sh -c 'if [ -n "${BASH_VERSION:-}" ]; then echo "BASH_VERSION_SET=$BASH_VERSION"; else echo BASH_VERSION_EMPTY; fi'

echo "--- TEST B: local builtin under /bin/sh"
/bin/sh -c 'f() { local x=1; echo "LOCAL_OK x=$x"; }; f' 2>&1 | head -n 2

# TEST C/D/E: echo -e vs echo vs printf.
# od -c is the arbiter: it shows a real TAB as \t and a literal two-char
# backslash-t as \ \ t. Byte counts are recorded as a cross-check, because
# dash echoes the "-e" itself while bash consumes it -- different byte counts.
echo "--- TEST C: echo -e (od -c)"
/bin/sh -c 'echo -e "a\tb"' 2>&1 | od -c | head -n 2
echo "--- TEST C: echo -e (byte count, includes trailing newline)"
/bin/sh -c 'echo -e "a\tb"' 2>&1 | wc -c

echo "--- TEST D: echo, no -e (od -c)"
/bin/sh -c 'echo "a\tb"' 2>&1 | od -c | head -n 2
echo "--- TEST D: echo, no -e (byte count)"
/bin/sh -c 'echo "a\tb"' 2>&1 | wc -c

echo "--- TEST E: printf (the portable way; identical everywhere)"
/bin/sh -c 'printf "a\tb\n"' 2>&1 | od -c | head -n 2
echo "--- TEST E: printf (byte count)"
/bin/sh -c 'printf "a\tb\n"' 2>&1 | wc -c

echo "--- TEST F: [[ ]] acceptance"
/bin/sh -c 'if [[ 1 -eq 1 ]]; then echo DOUBLE_BRACKET_OK; else echo DOUBLE_BRACKET_FAIL; fi' 2>&1 | head -n 3

echo "--- TEST G: source builtin vs . (POSIX)"
/bin/sh -c 'echo "echo SOURCE_RAN" >/tmp/li-0f-src.sh; source /tmp/li-0f-src.sh 2>/dev/null || echo SOURCE_NOT_AVAILABLE; . /tmp/li-0f-src.sh' 2>&1 | head -n 3

echo "--- TEST H: pipefail support under /bin/sh"
/bin/sh -c 'set -o pipefail 2>/dev/null && echo PIPEFAIL_SUPPORTED || echo PIPEFAIL_UNSUPPORTED'

echo "--- TEST I: set -e arithmetic / return quirk, dash vs bash"
/bin/sh -c 'x=$((0)); echo "ARITH_ZERO=$x"; false || echo OR_CHAIN_OK'

echo "--- TEST K: env var visibility of BASH_* inside /bin/sh"
/bin/sh -c 'env | grep -c "^BASH" || echo "no BASH* env vars"' 2>&1 | head -n 3
