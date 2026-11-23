#!/bin/sh
# 0-F step 3: procfs layout and kernel limits. POSIX sh only.
set -u

echo "--- uname (container shares the host kernel)"
uname -a

echo "--- /proc/self/stat field count"
awk '{print NF}' /proc/self/stat
echo "--- /proc/self/stat first 200 chars"
cut -c1-200 /proc/self/stat
echo "--- /proc/self/status selected fields"
grep -E '^(Name|State|Pid|PPid|Threads|Seccomp|NoNewPrivs|CapEff|CapBnd):' /proc/self/status

echo "--- kernel tunables"
for f in kernel/pid_max kernel/ns_last_pid kernel/threads-max kernel/hostname \
         fs/inotify/max_user_watches fs/inotify/max_user_instances \
         fs/inotify/max_queued_events fs/file-max fs/nr_open; do
    if [ -r "/proc/sys/$f" ]; then
        printf '%s = %s\n' "$f" "$(cat "/proc/sys/$f")"
    else
        printf '%s = UNREADABLE\n' "$f"
    fi
done

echo "--- ulimits"
printf 'ulimit -n = %s\n' "$(ulimit -n)"
printf 'ulimit -u = %s\n' "$(ulimit -u 2>/dev/null || echo unavailable)"
printf 'ulimit -s = %s\n' "$(ulimit -s 2>/dev/null || echo unavailable)"

echo "--- visible numeric /proc entries (PID namespace size)"
ls /proc | grep -c '^[0-9][0-9]*$'

echo "--- /proc/1 exe and cmdline"
ls -l /proc/1/exe 2>&1
printf 'cmdline: '; tr '\000' ' ' </proc/1/cmdline 2>/dev/null; echo

echo "--- cgroup version and limits as seen inside"
if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
    printf 'cgroup_version = v2\n'
    printf 'controllers    = %s\n' "$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null)"
    printf 'memory.max     = %s\n' "$(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo UNREADABLE)"
    printf 'cpu.max        = %s\n' "$(cat /sys/fs/cgroup/cpu.max 2>/dev/null || echo UNREADABLE)"
    printf 'pids.max       = %s\n' "$(cat /sys/fs/cgroup/pids.max 2>/dev/null || echo UNREADABLE)"
else
    printf 'cgroup_version = v1-or-unavailable\n'
fi

echo "--- namespaces"
ls -l /proc/self/ns/ 2>&1
