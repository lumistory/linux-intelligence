#!/bin/sh
# 0-F step 9: two measured traps that change how a probe script must be written.
#   A) `read < /proc/...` truncates to 1 byte under dash (st_size == 0)
#   B) `set -o pipefail` is NOT spec'd by POSIX and differs by dash patchlevel
set -u

echo "=== TRAP A: reading a procfs file with the read builtin"
printf 'shell reading it: %s\n' "$(readlink -f /proc/$$/exe 2>/dev/null)"

_a=""
read -r _a </proc/sys/kernel/pid_max 2>/dev/null || true
printf 'read -r v </proc/sys/kernel/pid_max  ->  [%s]\n' "${_a:-<empty>}"

_b="$(cat /proc/sys/kernel/pid_max 2>/dev/null)"
printf 'v=$(cat /proc/sys/kernel/pid_max)     ->  [%s]\n' "${_b:-<empty>}"

_size="$(stat -c %s /proc/sys/kernel/pid_max 2>/dev/null || echo '?')"
printf 'stat -c %%s on that procfs file      ->  [%s]\n' "$_size"

if [ "${_a:-}" = "${_b:-}" ]; then
    echo 'VERDICT_A: read builtin gives the SAME answer as cat (safe here)'
else
    echo 'VERDICT_A: read builtin TRUNCATES procfs data (must use cat)'
fi

echo
echo "=== TRAP B: set -o pipefail"
if /bin/sh -c 'set -o pipefail' 2>/dev/null; then
    echo 'accepts pipefail: yes'
    _r="$(/bin/sh -c 'set -o pipefail; false | true; echo $?' 2>/dev/null)"
    printf 'false | true with pipefail set -> exit %s (1 means pipefail is real)\n' "${_r:-?}"
    _opt="$(/bin/sh -c 'set -o pipefail 2>/dev/null; set -o 2>/dev/null | grep pipefail' 2>/dev/null)"
    printf 'option table says: [%s]\n' "${_opt:-<not listed>}"
else
    echo 'accepts pipefail: NO'
    /bin/sh -c 'set -o pipefail' 2>&1 | sed 's/^/    error: /'
    _r="$(/bin/sh -c 'false | true; echo $?' 2>/dev/null)"
    printf 'false | true without pipefail -> exit %s (0 = last command wins)\n' "${_r:-?}"
fi

echo
echo "=== TRAP C: which shell implements /bin/sh, and its package version"
printf 'readlink -f /bin/sh  ->  %s\n' "$(readlink -f /bin/sh 2>/dev/null)"
if command -v dpkg >/dev/null 2>&1; then
    dpkg -s dash 2>/dev/null | grep -E '^Version' | sed 's/^/    /' || true
elif command -v pacman >/dev/null 2>&1; then
    pacman -Q bash 2>/dev/null | sed 's/^/    /' || true
elif command -v rpm >/dev/null 2>&1; then
    rpm -q bash 2>/dev/null | sed 's/^/    /' || true
fi
