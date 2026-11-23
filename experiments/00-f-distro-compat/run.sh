#!/usr/bin/env bash
# Phase 0 / experiment 0-F driver: cross-distribution compatibility.
#
# Reproduces every number in REPORT.md.
#
#   * no sudo, no --privileged, no network, no image pulls (images are local)
#   * every podman call: `timeout 120` + `--rm`
#   * every step writes its own file under results/ ; the console only sees the
#     last 20 lines of each step
#   * idempotent: re-running overwrites results
#
# Usage: ./run.sh [--quick]
#   --quick   build artefacts + host probes only (skip containers)
#
# Exits non-zero if a driver step fails. A failed in-container capability is
# data, not a driver failure: it is captured and the driver continues.
set -euo pipefail

EXP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$EXP_DIR"

QUICK=0
for a in "$@"; do
  case "$a" in
    --quick) QUICK=1 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RAW="$EXP_DIR/results"
BIN="$RAW/bin"
PROBES="$EXP_DIR/src/probes"
mkdir -p "$RAW" "$BIN"

TAIL_LINES=20
PODMAN_TIMEOUT=120
IMG_ARCH="docker.io/library/archlinux:latest"
IMG_UBU="docker.io/library/ubuntu:24.04"
IMG_DEB="docker.io/library/debian:13"
IMG_FED="docker.io/library/fedora:42"
IMAGES=("$IMG_ARCH" "$IMG_UBU" "$IMG_DEB" "$IMG_FED")

label_for() {
  case "$1" in
    *archlinux*) echo arch ;;
    *ubuntu*)    echo ubuntu2404 ;;
    *debian*)    echo debian13 ;;
    *fedora*)    echo fedora42 ;;
    *) echo "$(printf '%s' "$1" | tr ':/.' '___')" ;;
  esac
}

echo "=== 0-F driver $STAMP"
echo "=== results -> $RAW"

# ---------------------------------------------------------------------------
# Stage 0 — build artefacts on the host
# ---------------------------------------------------------------------------
echo
echo "=== stage 0: build artefacts"

gcc -O2 -static -Wall -Wextra -o "$BIN/capcheck" "$EXP_DIR/src/capcheck.c" \
  >"$RAW/00_build.log" 2>&1

cat >"$BIN/hello.rs" <<'EOF'
fn main() { println!("hello-rust"); }
EOF
rustc -O -o "$BIN/hello_dyn" "$BIN/hello.rs" >>"$RAW/00_build.log" 2>&1
rustc -O -C target-feature=+crt-static -o "$BIN/hello_static" "$BIN/hello.rs" \
  >>"$RAW/00_build.log" 2>&1

cat >"$BIN/newer.c" <<'EOF'
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
int main(void) {
  char b[16];
  memset(b, 0, sizeof b);
  memset_explicit(b, 'x', 4);   /* glibc >= 2.43 */
  printf("hello-newer: %s\n", b);
  return 0;
}
EOF
gcc -O1 -o "$BIN/hello_newer" "$BIN/newer.c" >>"$RAW/00_build.log" 2>&1

# A dash binary extracted from debian:13 so the host can also run the probe
# under a non-bash POSIX shell (Arch has no dash).
timeout "$PODMAN_TIMEOUT" podman run --rm "$IMG_DEB" cat /usr/bin/dash \
  >"$BIN/dash" 2>>"$RAW/00_build.log" || true
chmod +x "$BIN/dash" 2>/dev/null || true

{
  echo "### build host: $(uname -n) kernel $(uname -r)"
  echo "### gcc: $(gcc --version | head -1)"
  echo "### rustc: $(rustc --version)"
  echo
  for f in capcheck hello_dyn hello_static hello_newer dash; do
    echo "--- $f"
    file "$BIN/$f" 2>&1
    echo "    size: $(stat -c %s "$BIN/$f" 2>/dev/null) bytes"
    printf '    GLIBC version nodes required: '
    objdump -T "$BIN/$f" 2>/dev/null |
      grep -o 'GLIBC_[0-9][0-9.]*' | sort -uV | tr '\n' ' ' || true
    echo
    printf '    exec on host: '
    case "$f" in
      dash) "$BIN/dash" -c 'echo dash-runs' 2>&1 | head -1 ;;
      *)     "$BIN/$f" 2>&1 | head -1 ;;
    esac
  done
} >"$RAW/00_build_artefacts.log" 2>&1
echo "--- build artefacts:"
tail -n "$TAIL_LINES" "$RAW/00_build_artefacts.log"

# ---------------------------------------------------------------------------
# Stage 1 — probe.sh under three shells on the host
# ---------------------------------------------------------------------------
echo
echo "=== stage 1: probe.sh on the host (bash, bash-as-sh, dash)"

{
  echo "### host probe under bash"
  echo "### ---"
  timeout 60 bash "$EXP_DIR/probe.sh" 2>&1 || echo "### [driver] exit: $?"
} >"$RAW/01_probe_host_bash.log"
timeout 60 bash "$EXP_DIR/probe.sh" >"$RAW/01_probe_host_bash.json" 2>/dev/null ||
  true
echo "--- host probe under bash: $(wc -l <"$RAW/01_probe_host_bash.json") lines"

{
  echo "### host probe under bash invoked as sh"
  echo "### ---"
  timeout 60 bash --posix "$EXP_DIR/probe.sh" 2>&1 || echo "### [driver] exit: $?"
} >"$RAW/01_probe_host_bash_posix.log"

if [ -x "$BIN/dash" ] && "$BIN/dash" -c true 2>/dev/null; then
  {
    echo "### host probe under dash (extracted from debian:13, runs on Arch)"
    echo "### ---"
    timeout 60 "$BIN/dash" "$EXP_DIR/probe.sh" 2>&1 || echo "### [driver] exit: $?"
  } >"$RAW/01_probe_host_dash.log"
  timeout 60 "$BIN/dash" "$EXP_DIR/probe.sh" >"$RAW/01_probe_host_dash.json" \
    2>/dev/null || true
  echo "--- host probe under dash: $(wc -l <"$RAW/01_probe_host_dash.json") lines"
else
  echo "### dash extracted from debian:13 could not be executed on this host" \
    >"$RAW/01_probe_host_dash.log"
  echo "--- host probe under dash: NOT TESTED (dash binary unusable)"
fi

{
  echo "### host binary capability check (no container)"
  echo "### ---"
  timeout 60 "$BIN/capcheck" 2>&1
} >"$RAW/01_host_capcheck.log"

if [ "$QUICK" = "1" ]; then
  echo
  echo "=== --quick: stopping before container stages"
  echo "=== done"
  exit 0
fi

# ---------------------------------------------------------------------------
# Stage 2 — per-distribution steps
# ---------------------------------------------------------------------------
# run_step <image> <probe script> <result basename>
# Bind-mounts the probe dir read-only at /probe and results/bin at /probe-bin.
run_step() {
  local image="$1" script="$2" outbase="$3" label outfile
  label="$(label_for "$image")"
  outfile="$RAW/${outbase}_${label}.log"
  {
    echo "### image:  $image"
    echo "### utc:    $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "### script: src/probes/$script"
    echo "### cmd:    timeout $PODMAN_TIMEOUT podman run --rm --network none" \
         "-v src/probes:/probe:ro -v results/bin:/probe-bin:ro $image sh /probe/$script"
    echo "### ---"
    # shellcheck disable=SC2086
    timeout "$PODMAN_TIMEOUT" podman run --rm --network none \
      -v "$PROBES:/probe:ro" \
      -v "$BIN:/probe-bin:ro" \
      "$image" sh "/probe/$script" 2>&1 ||
      echo "### [driver] container exited non-zero (see above)"
    echo "### done"
  } >"$outfile"
  echo "--- ${outbase}_${label}.log"
  tail -n "$TAIL_LINES" "$outfile"
}

for image in "${IMAGES[@]}"; do
  echo
  echo "=================================================================="
  echo "=== $(label_for "$image")  ($image)"
  echo "=================================================================="

  run_step "$image" p1_ident.sh 10_ident
  run_step "$image" p2_shell.sh 11_shell
  run_step "$image" p3_procfs.sh 12_procfs
  run_step "$image" p4_capability.sh 13_capability
  run_step "$image" p5_glibc.sh 14_glibc
  run_step "$image" p6_recovery.sh 15_recovery
  run_step "$image" p7_init.sh 16_initdetect
  run_step "$image" p8_dist.sh 17_distbins
done

# ---------------------------------------------------------------------------
# Stage 3 — probe.sh inside every container
# ---------------------------------------------------------------------------
echo
echo "=== stage 3: probe.sh inside every container"

run_probe_in() {
  local image="$1" label outfile
  label="$(label_for "$image")"
  outfile="$RAW/20_probe_${label}.log"
  {
    echo "### image: $image"
    echo "### utc:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "### ---"
    timeout "$PODMAN_TIMEOUT" podman run --rm --network none \
      -v "$EXP_DIR/probe.sh:/probe.sh:ro" \
      "$image" sh /probe.sh 2>&1 || echo "### [driver] exit: $?"
    echo "### done"
  } >"$outfile"
  # extract the JSON body (drop the ### framing lines)
  sed -n '/^{$/,/^}$/p' "$outfile" >"$RAW/20_probe_${label}.json"
  if jq -e . "$RAW/20_probe_${label}.json" >/dev/null 2>&1; then
    echo "--- 20_probe_${label}.json  VALID JSON"
  else
    echo "--- 20_probe_${label}.json  INVALID JSON (see .log)"
  fi
}

for image in "${IMAGES[@]}"; do
  run_probe_in "$image"
done

# ---------------------------------------------------------------------------
# Stage 4 — cross-distribution consistency matrix from the probe JSONs
# ---------------------------------------------------------------------------
echo
echo "=== stage 4: consistency matrix"

{
  echo "### Field-by-field comparison of probe.sh output across 4 containers + host"
  echo
  printf '%-34s %-14s %-14s %-14s %-14s\n' \
    FIELD arch ubuntu2404 debian13 fedora42
  for jf in \
    '.os_release.ID' '.os_release.ID_LIKE' '.os_release.VERSION_ID' \
    '.os_release.pkg_manager' \
    '.init.pid1_comm' '.init.pid1_is_systemd' '.init.systemctl_present' \
    '.shells.bin_sh_impl' '.shells.bash_version' '.probe_under_bash' \
    '.libc.glibc_version' \
    '.procfs.self_stat_fields' '.procfs.pid_max' \
    '.inotify_limits.max_user_watches' \
    '.inotify_limits.max_user_instances' \
    '.inotify_limits.max_queued_events' \
    '.recovery_tools.btrfs' '.recovery_tools.snapper' \
    '.recovery_tools.git'; do
    a=$(jq -r "$jf // \"-\"" "$RAW/20_probe_arch.json" 2>/dev/null || echo ERR)
    u=$(jq -r "$jf // \"-\"" "$RAW/20_probe_ubuntu2404.json" 2>/dev/null || echo ERR)
    d=$(jq -r "$jf // \"-\"" "$RAW/20_probe_debian13.json" 2>/dev/null || echo ERR)
    f=$(jq -r "$jf // \"-\"" "$RAW/20_probe_fedora42.json" 2>/dev/null || echo ERR)
    printf '%-34s %-14s %-14s %-14s %-14s\n' \
      "${jf#.}" "${a:--}" "${u:--}" "${d:--}" "${f:--}"
  done
  echo
  echo "### which fields differ between the four containers?"
  for jf in '.os_release.ID' '.os_release.VERSION_ID' '.os_release.pkg_manager' \
            '.init.pid1_comm' '.init.systemctl_present' '.shells.bin_sh_impl' \
            '.shells.bash_version' '.libc.glibc_version' \
            '.procfs.self_stat_fields' '.procfs.pid_max' \
            '.inotify_limits.max_user_watches'; do
    a=$(jq -r "$jf // \"-\"" "$RAW/20_probe_arch.json" 2>/dev/null)
    u=$(jq -r "$jf // \"-\"" "$RAW/20_probe_ubuntu2404.json" 2>/dev/null)
    d=$(jq -r "$jf // \"-\"" "$RAW/20_probe_debian13.json" 2>/dev/null)
    f=$(jq -r "$jf // \"-\"" "$RAW/20_probe_fedora42.json" 2>/dev/null)
    if [ "$a" = "$u" ] && [ "$a" = "$d" ] && [ "$a" = "$f" ]; then
      printf 'SAME      %-32s = %s\n' "${jf#.}" "$a"
    else
      printf 'DIFFERS   %-32s arch=%s ubuntu=%s debian=%s fedora=%s\n' \
        "${jf#.}" "$a" "$u" "$d" "$f"
    fi
  done

  echo
  echo "### host vs arch container: kernel-derived fields must be identical"
  if [ -f "$RAW/01_probe_host_bash.json" ]; then
    for jf in '.host.uname_r' '.procfs.self_stat_fields' '.procfs.pid_max' \
              '.inotify_limits.max_user_watches' \
              '.inotify_limits.max_user_instances' \
              '.inotify_limits.max_queued_events'; do
      h=$(jq -r "$jf // \"-\"" "$RAW/01_probe_host_bash.json" 2>/dev/null)
      a=$(jq -r "$jf // \"-\"" "$RAW/20_probe_arch.json" 2>/dev/null)
      if [ "$h" = "$a" ]; then
        printf 'SAME      %-32s host=%s container-arch=%s\n' "${jf#.}" "$h" "$a"
      else
        printf 'DIFFERS   %-32s host=%s container-arch=%s\n' "${jf#.}" "$h" "$a"
      fi
    done
    for jf in '.init.pid1_comm' '.init.pid1_is_systemd'; do
      h=$(jq -r "$jf // \"-\"" "$RAW/01_probe_host_bash.json" 2>/dev/null)
      a=$(jq -r "$jf // \"-\"" "$RAW/20_probe_arch.json" 2>/dev/null)
      printf 'CONTRAST  %-32s host=%s container=%s\n' "${jf#.}" "$h" "$a"
    done
  fi
} >"$RAW/30_matrix.log" 2>&1
echo "--- 30_matrix.log"
tail -n "$TAIL_LINES" "$RAW/30_matrix.log"

# ---------------------------------------------------------------------------
# Stage 5 — distribution test summary
# ---------------------------------------------------------------------------
echo
echo "=== stage 5: binary distribution summary"

{
  echo "### Can one binary built on Arch (glibc $(ldd --version | head -1 | awk '{print $NF}')) run everywhere?"
  echo
  printf '%-12s %-10s %-24s %-10s %-24s\n' \
    DISTRO hello_dyn hello_static hello_newer "libc_seen"
  for image in "${IMAGES[@]}"; do
    label="$(label_for "$image")"
    f="$RAW/17_distbins_${label}.log"
    [ -f "$f" ] || continue
    dyn=$(awk '/hello_dyn exit =/{print $3}' "$f" | head -1)
    sta=$(awk '/hello_static exit =/{print $3}' "$f" | head -1)
    new=$(awk '/hello_newer exit =/{print $3}' "$f" | head -1)
    libc=$(sed -n 's/^### ---$//;1,200p' "$f" | grep -m1 -o 'GLIBC [0-9.]*' || true)
    lic=$(grep -m1 -E '^ldd ' "$RAW/14_glibc_${label}.log" | awk '{print $NF}')
    printf '%-12s %-10s %-24s %-10s %-24s\n' \
      "$label" "${dyn:-?}" "${sta:-?}(static)" "${new:-?}" "${lic:-?}"
  done
  echo
  echo "### loader error messages (verbatim, from results/17_distbins_*.log):"
  grep -h -E 'not found|error while loading|cannot execute' \
    "$RAW"/17_distbins_*.log 2>/dev/null | sort -u || echo "  (none)"
} >"$RAW/31_dist_matrix.log" 2>&1
echo "--- 31_dist_matrix.log"
tail -n "$TAIL_LINES" "$RAW/31_dist_matrix.log"

# ---------------------------------------------------------------------------
# Stage 6 — init-detection evidence table
# ---------------------------------------------------------------------------
echo
echo "=== stage 6: init detection evidence"

{
  echo "### Does the systemctl binary correctly indicate a running systemd?"
  printf '%-12s %-14s %-16s %-12s %-14s\n' \
    DISTRO systemctl_bin is-system-running pid1_comm VERDICT
  for image in "${IMAGES[@]}"; do
    label="$(label_for "$image")"
    f="$RAW/16_initdetect_${label}.log"
    [ -f "$f" ] || continue
    present=$(grep -m1 -o 'systemctl: PRESENT' "$f" >/dev/null 2>&1 &&
      echo PRESENT || echo ABSENT)
    running=$(grep -A1 'is-system-running ->' "$f" | tail -1 | cut -c1-24)
    comm=$(sed -n 's/^pid1 comm: *//p' "$f" | head -1)
    verdict=$(sed -n 's/^systemd_running = //p' "$f" | head -1)
    printf '%-12s %-14s %-16s %-12s %-14s\n' \
      "$label" "$present" "${running:--}" "${comm:--}" "${verdict:--}"
  done
} >"$RAW/32_initdetect_matrix.log" 2>&1
echo "--- 32_initdetect_matrix.log"
tail -n "$TAIL_LINES" "$RAW/32_initdetect_matrix.log"

# ---------------------------------------------------------------------------
# Stage 7 — index of every result file
# ---------------------------------------------------------------------------
echo
echo "=== stage 7: results index"
{
  echo "# results index ($STAMP)"
  echo
  printf '%-40s %10s\n' FILE BYTES
  for f in "$RAW"/*.log "$RAW"/*.json; do
    [ -f "$f" ] || continue
    printf '%-40s %10s\n' "$(basename "$f")" "$(stat -c %s "$f")"
  done
  echo
  echo "total files: $(ls -1 "$RAW" | wc -l)"
} >"$RAW/99_index.log" 2>&1
tail -n "$TAIL_LINES" "$RAW/99_index.log"

echo
echo "=== 0-F driver complete"
