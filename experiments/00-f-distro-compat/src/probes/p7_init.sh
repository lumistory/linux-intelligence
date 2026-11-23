#!/bin/sh
# 0-F step 7: prove the correct systemd detection method.
set -u

echo "=== METHOD A (WRONG): does the systemctl binary exist?"
if command -v systemctl >/dev/null 2>&1; then
    printf 'systemctl: PRESENT at %s\n' "$(command -v systemctl)"
    printf 'systemctl --version: '
    systemctl --version 2>&1 | head -n 1
else
    printf 'systemctl: ABSENT (binary test happens to agree with reality here)\n'
fi

echo "=== METHOD B (WRONG): ask systemctl about system state"
if command -v systemctl >/dev/null 2>&1; then
    printf 'is-system-running -> '
    systemctl is-system-running 2>&1 | head -n 2 || echo "(failed)"
    printf 'list-units rc -> '
    systemctl list-units --no-pager >/dev/null 2>&1; echo $?
else
    echo "skipped (no systemctl binary)"
fi

echo "=== METHOD C (HEURISTIC): does /run/systemd/system exist?"
ls -ld /run/systemd/system 2>&1

echo "=== METHOD D (CORRECT): is PID 1 systemd?"
printf 'pid1 comm:    %s\n' "$(cat /proc/1/comm 2>/dev/null)"
printf 'pid1 exe:     %s\n' "$(readlink -f /proc/1/exe 2>/dev/null)"
printf 'pid1 cmdline: '; tr '\000' ' ' </proc/1/cmdline 2>/dev/null; echo

echo "=== VERDICT"
if [ "$(cat /proc/1/comm 2>/dev/null)" = "systemd" ]; then
    echo "systemd_running = true  (PID 1 is systemd)"
else
    echo "systemd_running = false (PID 1 is NOT systemd; the binary lies)"
fi

echo "=== unit directories exist even without a running systemd?"
ls -d /usr/lib/systemd/system /etc/systemd/system /lib/systemd/system 2>&1 | head -n 4
