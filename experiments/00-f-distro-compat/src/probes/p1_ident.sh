#!/bin/sh
# 0-F step 1: base identification. POSIX sh only.
set -u

echo "--- /etc/os-release (raw)"
cat /etc/os-release 2>&1

echo "--- selected fields"
for k in ID ID_LIKE VERSION_ID PRETTY_NAME NAME BUILD_ID; do
    v="$(sed -n "s/^$k=//p" /etc/os-release 2>/dev/null | head -n 1 | sed 's/^"//; s/"$//')"
    printf '%s=%s\n' "$k" "$v"
done

echo "--- package managers"
for p in pacman apt apt-get dpkg dnf rpm zypper apk yum; do
    if command -v "$p" >/dev/null 2>&1; then
        printf '%s: %s\n' "$p" "$(command -v "$p")"
    else
        printf '%s: ABSENT\n' "$p"
    fi
done

echo "--- /bin/sh"
ls -l /bin/sh 2>&1
printf 'readlink -f /bin/sh -> %s\n' "$(readlink -f /bin/sh 2>/dev/null)"

echo "--- pid 1"
printf 'comm:    %s\n' "$(cat /proc/1/comm 2>/dev/null)"
printf 'cmdline: '; tr '\000' ' ' </proc/1/cmdline 2>/dev/null; echo

echo "--- container markers"
ls -l /run/.containerenv /.dockerenv 2>&1

echo "--- user / arch"
id 2>&1
uname -m
