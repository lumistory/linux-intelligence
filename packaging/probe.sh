#!/bin/sh
# Linux Intelligence 主机能力探测器（Phase 6: P6-02 .. P6-04，严格 POSIX sh）
# 约束：
# - 严禁 bashism：不使用 local / [[ ]] / source / 数组 / echo -e / set -o pipefail
# - 读取 procfs 一律使用 cat，不使用 `read < procfile`（P6-03：dash 下会被截断为 1 字节）
# - init 系统只认 /proc/1/comm == systemd（P6-04：systemctl 存在不等于作为 PID 1 运行）

set -eu

json_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n' ' '
}

has_cmd() {
    if command -v "$1" >/dev/null 2>&1; then
        printf 'true'
    else
        printf 'false'
    fi
}

read_proc_trim() {
    if [ -r "$1" ]; then
        cat "$1" 2>/dev/null | tr -d '\r\n'
    else
        printf 'unknown'
    fi
}

SH_TARGET="unknown"
if command -v readlink >/dev/null 2>&1 && [ -L /bin/sh ]; then
    SH_TARGET=$(readlink -f /bin/sh 2>/dev/null || readlink /bin/sh 2>/dev/null || printf 'unknown')
elif [ -x /bin/sh ]; then
    SH_TARGET="/bin/sh"
fi

KERNEL_REL=$(uname -r 2>/dev/null || printf 'unknown')
ARCH=$(uname -m 2>/dev/null || printf 'unknown')
UID_VAL=$(id -u 2>/dev/null || printf 'unknown')
INIT_COMM=$(read_proc_trim /proc/1/comm)
SYSTEMD_INIT="false"
if [ "$INIT_COMM" = "systemd" ]; then
    SYSTEMD_INIT="true"
fi

PID_MAX=$(read_proc_trim /proc/sys/kernel/pid_max)
INOTIFY_WATCHES=$(read_proc_trim /proc/sys/fs/inotify/max_user_watches)
NOFILE_SOFT=$(ulimit -Sn 2>/dev/null || printf 'unknown')

GLIBC_VER="absent"
if command -v getconf >/dev/null 2>&1; then
    GLIBC_VER=$(getconf GNU_LIBC_VERSION 2>/dev/null || printf 'unknown')
elif command -v ldd >/dev/null 2>&1; then
    GLIBC_VER=$(ldd --version 2>&1 | head -n 1 || printf 'unknown')
fi

HIDEPID="absent"
if [ -r /proc/mounts ]; then
    HIDEPID_VAL=$(cat /proc/mounts 2>/dev/null | grep -E ' /proc proc ' | grep -o 'hidepid=[^ ,]*' | head -n 1 || true)
    if [ -n "$HIDEPID_VAL" ]; then
        HIDEPID="$HIDEPID_VAL"
    fi
fi

CMD_BTRFS=$(has_cmd btrfs)
CMD_MKFS_BTRFS=$(has_cmd mkfs.btrfs)
CMD_SNAPPER=$(has_cmd snapper)
CMD_LVS=$(has_cmd lvs)
CMD_LVCREATE=$(has_cmd lvcreate)
CMD_GIT=$(has_cmd git)
CMD_SETSID=$(has_cmd setsid)
CMD_STRACE=$(has_cmd strace)
CMD_SYSTEMCTL=$(has_cmd systemctl)
CMD_BASH=$(has_cmd bash)
CMD_ZSH=$(has_cmd zsh)
CMD_DASH=$(has_cmd dash)

RECOVERY_PROVIDER="manual"
if [ "$CMD_GIT" = "true" ]; then
    RECOVERY_PROVIDER="git+manual"
fi
if [ "$CMD_BTRFS" = "true" ] && [ "$CMD_MKFS_BTRFS" = "true" ]; then
    RECOVERY_PROVIDER="btrfs-image+$RECOVERY_PROVIDER"
fi

printf '{\n'
printf '  "schema_version": 1,\n'
printf '  "kernel_release": "%s",\n' "$(json_escape "$KERNEL_REL")"
printf '  "arch": "%s",\n' "$(json_escape "$ARCH")"
printf '  "uid": "%s",\n' "$(json_escape "$UID_VAL")"
printf '  "bin_sh_target": "%s",\n' "$(json_escape "$SH_TARGET")"
printf '  "init_comm": "%s",\n' "$(json_escape "$INIT_COMM")"
printf '  "systemd_is_pid1": %s,\n' "$SYSTEMD_INIT"
printf '  "pid_max": "%s",\n' "$(json_escape "$PID_MAX")"
printf '  "inotify_max_user_watches": "%s",\n' "$(json_escape "$INOTIFY_WATCHES")"
printf '  "nofile_soft": "%s",\n' "$(json_escape "$NOFILE_SOFT")"
printf '  "proc_hidepid": "%s",\n' "$(json_escape "$HIDEPID")"
printf '  "libc": "%s",\n' "$(json_escape "$GLIBC_VER")"
printf '  "recommended_recovery_chain": "%s",\n' "$(json_escape "$RECOVERY_PROVIDER")"
printf '  "tools": {\n'
printf '    "btrfs": %s,\n' "$CMD_BTRFS"
printf '    "mkfs_btrfs": %s,\n' "$CMD_MKFS_BTRFS"
printf '    "snapper": %s,\n' "$CMD_SNAPPER"
printf '    "lvs": %s,\n' "$CMD_LVS"
printf '    "lvcreate": %s,\n' "$CMD_LVCREATE"
printf '    "git": %s,\n' "$CMD_GIT"
printf '    "setsid": %s,\n' "$CMD_SETSID"
printf '    "strace": %s,\n' "$CMD_STRACE"
printf '    "systemctl": %s,\n' "$CMD_SYSTEMCTL"
printf '    "bash": %s,\n' "$CMD_BASH"
printf '    "zsh": %s,\n' "$CMD_ZSH"
printf '    "dash": %s\n' "$CMD_DASH"
printf '  }\n'
printf '}\n'
