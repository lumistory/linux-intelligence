#!/bin/sh
# 0-F step 4: what the container can actually DO (real syscalls, seccomp).
set -u

echo "--- capcheck: static binary, real syscalls"
/probe-bin/capcheck
printf 'capcheck exit: %s\n' "$?"

echo "--- our own seccomp / capability status"
grep -E '^(Seccomp|NoNewPrivs|CapEff|CapBnd|CapPrm):' /proc/self/status 2>&1

echo "--- ls /proc (plain)"
ls /proc 2>&1 | head -n 5
printf 'ls /proc exit: %s\n' "$?"

echo "--- sockets / tools available for a second opinion"
for t in socat nc ncat python3 perl; do
    if command -v "$t" >/dev/null 2>&1; then
        printf '%s: %s\n' "$t" "$(command -v "$t")"
    else
        printf '%s: ABSENT\n' "$t"
    fi
done

echo "--- can we write to /tmp at all?"
if : >/tmp/li-0f-write-test; then echo "tmp_writable = yes"; rm -f /tmp/li-0f-write-test; else echo "tmp_writable = no"; fi

echo "--- /proc/sys is it writable from the container?"
if [ -w /proc/sys/fs/inotify/max_user_watches ]; then echo "sysctl_writable = yes"; else echo "sysctl_writable = no"; fi

echo "--- mounted filesystems (is /proc a real procfs?)"
grep -E ' /proc | /sys | /dev ' /proc/mounts 2>&1 | head -n 6
