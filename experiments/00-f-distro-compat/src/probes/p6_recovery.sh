#!/bin/sh
# 0-F step 6: recovery tooling availability (btrfs, snapper, lvm, git).
set -u

echo "--- tool presence (PATH then fixed bin dirs)"
for t in btrfs mkfs.btrfs snapper lvm lvm2 pvcreate vgcreate vgs lvs \
         git rsync tar zfs zpool xfs_info tune2fs; do
    if command -v "$t" >/dev/null 2>&1; then
        printf '%s: %s\n' "$t" "$(command -v "$t")"
    else
        found=""
        for d in /usr/bin /usr/sbin /bin /sbin; do
            if [ -x "$d/$t" ]; then found="$d/$t"; break; fi
        done
        printf '%s: %s\n' "$t" "${found:-ABSENT}"
    fi
done

echo "--- filesystems the kernel offers (shared with host)"
tr '\n' ' ' </proc/filesystems 2>/dev/null; echo

echo "--- snapshot-capable filesystem actually mounted here?"
mount 2>/dev/null | grep -E 'btrfs|zfs|xfs' || echo "none of btrfs/zfs/xfs mounted"

echo "--- df"
df -h 2>&1 | head -n 5

echo "--- installed package count (metadata only, no network)"
if command -v pacman >/dev/null 2>&1; then printf 'pacman -Q: %s\n' "$(pacman -Q 2>/dev/null | wc -l)"; fi
if command -v dpkg >/dev/null 2>&1; then printf 'dpkg -l ii: %s\n' "$(dpkg -l 2>/dev/null | grep -c '^ii')"; fi
if command -v rpm >/dev/null 2>&1; then printf 'rpm -qa: %s\n' "$(rpm -qa 2>/dev/null | wc -l)"; fi

echo "--- can we even SEE a block device?"
ls -l /dev/sd* /dev/nvme* /dev/vd* 2>&1 | head -n 4
echo "--- /sys/block"
ls /sys/block 2>&1
