#!/bin/sh
# probe.sh -- Phase 0 / 0-F capability probe.
#
# STRICT POSIX sh. No bashisms: no [[ ]], no local, no $BASH_VERSION,
# no echo -e, no arrays, no pipefail. Must run identically under
# dash (Debian/Ubuntu /bin/sh) and bash-as-sh (Fedora/Arch /bin/sh).
#
# Emits exactly one JSON object on stdout. Diagnostics (if any) go to stderr,
# so the output stays machine-parseable.
#
# Usage: sh probe.sh

set -u

# ---------------------------------------------------------------- helpers

# json_str <value> -- print a JSON string literal, sanitised.
json_str() {
    printf '"%s"' "$(printf '%s' "$1" | tr -d '\\"' | tr -d '\000-\037')"
}

json_bool() {
    if [ "$1" = "1" ]; then printf 'true'; else printf 'false'; fi
}

# have <command> -> prints path or empty
have() {
    command -v "$1" 2>/dev/null || true
}

# rd <file> -> first line of file, or empty string if unreadable
rd() {
    if [ -r "$1" ]; then
        { IFS= read -r _line <"$1"; } 2>/dev/null || _line=""
        printf '%s' "$_line"
    else
        printf ''
    fi
}

# osr <key> -- read a key from /etc/os-release without sourcing it.
osr() {
    if [ -r /etc/os-release ]; then
        sed -n "s/^$1=//p" /etc/os-release | head -n 1 | sed 's/^"//; s/"$//'
    else
        printf ''
    fi
}

# ---------------------------------------------------------------- 1. identity

uname_r="$(uname -r 2>/dev/null || printf '')"
uname_m="$(uname -m 2>/dev/null || printf '')"

is_container=0
if [ -f /run/.containerenv ] || [ -f /.dockerenv ]; then is_container=1; fi

# ---------------------------------------------------------------- 2. os-release

os_id="$(osr ID)"
os_id_like="$(osr ID_LIKE)"
os_version_id="$(osr VERSION_ID)"
os_pretty="$(osr PRETTY_NAME)"
os_release_file=0
[ -r /etc/os-release ] && os_release_file=1

# ---------------------------------------------------------------- 3. init

pid1_comm="$(rd /proc/1/comm)"
pid1_is_systemd=0
[ "$pid1_comm" = "systemd" ] && pid1_is_systemd=1

systemctl_present=0
[ -n "$(have systemctl)" ] && systemctl_present=1

# Does /run/systemd/system exist? systemd creates it; a container with the
# binary installed but no systemd running will not have it.
run_systemd_dir=0
[ -d /run/systemd/system ] && run_systemd_dir=1

# systemctl is-system-running: only meaningful if pid1 is systemd.
if [ "$systemctl_present" = "1" ]; then
    sys_state="$(systemctl is-system-running 2>/dev/null || true)"
    [ -n "$sys_state" ] || sys_state="error-or-empty"
else
    sys_state="not-installed"
fi

# ---------------------------------------------------------------- 4. shells

sh_path="$(readlink -f /bin/sh 2>/dev/null || printf '/bin/sh')"
sh_impl="unknown"
case "$sh_path" in
    */dash) sh_impl="dash" ;;
    */bash) sh_impl="bash" ;;
    */busybox) sh_impl="busybox-ash" ;;
    */ash)   sh_impl="ash" ;;
esac

# The probe itself is running under /bin/sh; report what that shell says.
probe_sh_is_bash=0
[ -n "${BASH_VERSION:-}" ] && probe_sh_is_bash=1

bash_version=""
if [ -n "$(have bash)" ]; then
    bash_version="$(bash -c 'printf %s "$BASH_VERSION"' 2>/dev/null || true)"
fi

zsh_present=0; [ -n "$(have zsh)" ] && zsh_present=1
fish_present=0; [ -n "$(have fish)" ] && fish_present=1
dash_present=0; [ -n "$(have dash)" ] && dash_present=1

# ---------------------------------------------------------------- 5. glibc

glibc_line=""
if [ -n "$(have ldd)" ]; then
    glibc_line="$(ldd --version 2>&1 | head -n 1 | tr -d '\r')"
fi
# glibc version is the last whitespace-separated token of the first line.
glibc_version="$(printf '%s' "$glibc_line" | awk '{print $NF}')"

musl_present=0
[ -e /lib/ld-musl-x86_64.so.1 ] && musl_present=1

# ---------------------------------------------------------------- 6. procfs

stat_fields=""
if [ -r /proc/self/stat ]; then
    stat_fields="$(awk '{print NF}' /proc/self/stat 2>/dev/null || true)"
fi

pid_max="$(rd /proc/sys/kernel/pid_max)"
ns_last_pid=""
if [ -r /proc/sys/kernel/ns_last_pid ]; then ns_last_pid="$(rd /proc/sys/kernel/ns_last_pid)"; fi

procs_visible="$(ls /proc 2>/dev/null | grep -c '^[0-9][0-9]*$' || true)"

# ---------------------------------------------------------------- 7. inotify

in_max_watches="$(rd /proc/sys/fs/inotify/max_user_watches)"
in_max_instances="$(rd /proc/sys/fs/inotify/max_user_instances)"
in_max_queued="$(rd /proc/sys/fs/inotify/max_queued_events)"

# ---------------------------------------------------------------- 8. recovery

find_tool() {
    # search a small fixed set of bin dirs, do not trust PATH alone
    for d in /usr/bin /usr/sbin /bin /sbin /usr/local/bin /usr/local/sbin; do
        if [ -x "$d/$1" ]; then printf '%s' "$d/$1"; return 0; fi
    done
    printf ''
}

btrfs_p="$(find_tool btrfs)"
snapper_p="$(find_tool snapper)"
lvm_p="$(find_tool lvm)"
git_p="$(find_tool git)"
pkgmgr=""
for p in pacman apt dnf zypper apk; do
    if [ -n "$(have $p)" ]; then pkgmgr="$p"; break; fi
done

# ---------------------------------------------------------------- 9. cgroup /

cgroup_v2=0
if [ -f /sys/fs/cgroup/cgroup.controllers ]; then cgroup_v2=1; fi

# ---------------------------------------------------------------- emit JSON

printf '{\n'
printf '  "probe_schema": "li-cap/1",\n'
printf '  "probe_shell": %s,\n' "$(json_str "$(readlink -f /proc/$$/exe 2>/dev/null || printf 'unknown')")"
printf '  "probe_under_bash": %s,\n' "$(json_bool "$probe_sh_is_bash")"
printf '  "host": {\n'
printf '    "uname_r": %s,\n' "$(json_str "$uname_r")"
printf '    "uname_m": %s,\n' "$(json_str "$uname_m")"
printf '    "is_container": %s,\n' "$(json_bool "$is_container")"
printf '    "cgroup_v2": %s\n' "$(json_bool "$cgroup_v2")"
printf '  },\n'
printf '  "os_release": {\n'
printf '    "file_present": %s,\n' "$(json_bool "$os_release_file")"
printf '    "ID": %s,\n' "$(json_str "$os_id")"
printf '    "ID_LIKE": %s,\n' "$(json_str "$os_id_like")"
printf '    "VERSION_ID": %s,\n' "$(json_str "$os_version_id")"
printf '    "PRETTY_NAME": %s,\n' "$(json_str "$os_pretty")"
printf '    "pkg_manager": %s\n' "$(json_str "$pkgmgr")"
printf '  },\n'
printf '  "init": {\n'
printf '    "pid1_comm": %s,\n' "$(json_str "$pid1_comm")"
printf '    "pid1_is_systemd": %s,\n' "$(json_bool "$pid1_is_systemd")"
printf '    "systemctl_present": %s,\n' "$(json_bool "$systemctl_present")"
printf '    "run_systemd_dir": %s,\n' "$(json_bool "$run_systemd_dir")"
printf '    "systemctl_is_system_running": %s\n' "$(json_str "$sys_state")"
printf '  },\n'
printf '  "shells": {\n'
printf '    "bin_sh_target": %s,\n' "$(json_str "$sh_path")"
printf '    "bin_sh_impl": %s,\n' "$(json_str "$sh_impl")"
printf '    "bash_version": %s,\n' "$(json_str "$bash_version")"
printf '    "dash_present": %s,\n' "$(json_bool "$dash_present")"
printf '    "zsh_present": %s,\n' "$(json_bool "$zsh_present")"
printf '    "fish_present": %s\n' "$(json_bool "$fish_present")"
printf '  },\n'
printf '  "libc": {\n'
printf '    "ldd_first_line": %s,\n' "$(json_str "$glibc_line")"
printf '    "glibc_version": %s,\n' "$(json_str "$glibc_version")"
printf '    "musl_ldso_present": %s\n' "$(json_bool "$musl_present")"
printf '  },\n'
printf '  "procfs": {\n'
printf '    "self_stat_fields": %s,\n' "${stat_fields:-null}"
printf '    "pid_max": %s,\n' "${pid_max:-null}"
printf '    "ns_last_pid": %s,\n' "${ns_last_pid:-null}"
printf '    "visible_numeric_proc_entries": %s\n' "${procs_visible:-0}"
printf '  },\n'
printf '  "inotify_limits": {\n'
printf '    "max_user_watches": %s,\n' "${in_max_watches:-null}"
printf '    "max_user_instances": %s,\n' "${in_max_instances:-null}"
printf '    "max_queued_events": %s\n' "${in_max_queued:-null}"
printf '  },\n'
printf '  "recovery_tools": {\n'
printf '    "btrfs": %s,\n' "$(json_str "$btrfs_p")"
printf '    "snapper": %s,\n' "$(json_str "$snapper_p")"
printf '    "lvm": %s,\n' "$(json_str "$lvm_p")"
printf '    "git": %s\n' "$(json_str "$git_p")"
printf '  }\n'
printf '}\n'
