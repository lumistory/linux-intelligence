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

# jget <jsonfile> <jq-path> -- print the value, "-" only when absent/null.
# NOTE: `jq -r "$p // \"-\""` is WRONG here: jq's // also rewrites `false`.
jget() {
  local out
  out="$(jq -r "if (($2) == null) then \"-\" else (($2)|tostring) end" "$1" 2>/dev/null)" || {
    printf 'ERR'
    return 0
  }
  printf '%s' "$out"
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

PROBE_JSONS=()
for image in "${IMAGES[@]}"; do
  PROBE_JSONS+=("$RAW/20_probe_$(label_for "$image").json")
done

{
  echo "### probe.sh output, field by field: 4 containers (+ host for contrast)"
  echo "### source: results/20_probe_<distro>.json and results/01_probe_host_bash.json"
  echo
  printf '%-32s %-13s %-13s %-13s %-13s %-13s\n' \
    FIELD arch ubuntu2404 debian13 fedora42 HOST

  FIELDS='
.os_release.ID
.os_release.ID_LIKE
.os_release.VERSION_ID
.os_release.pkg_manager
.init.pid1_comm
.init.pid1_is_systemd
.init.systemctl_present
.init.run_systemd_dir
.shells.bin_sh_impl
.shells.bash_version
.probe_under_bash
.libc.glibc_version
.procfs.self_stat_fields
.procfs.pid_max
.inotify_limits.max_user_watches
.inotify_limits.max_user_instances
.inotify_limits.max_queued_events
.recovery_tools.btrfs
.recovery_tools.snapper
.recovery_tools.lvm
.recovery_tools.git'

  for jf in $FIELDS; do
    a="$(jget "$RAW/20_probe_arch.json" "$jf")"
    u="$(jget "$RAW/20_probe_ubuntu2404.json" "$jf")"
    d="$(jget "$RAW/20_probe_debian13.json" "$jf")"
    f="$(jget "$RAW/20_probe_fedora42.json" "$jf")"
    h="$(jget "$RAW/01_probe_host_bash.json" "$jf")"
    printf '%-32s %-13s %-13s %-13s %-13s %-13s\n' \
      "${jf#.}" "$a" "$u" "$d" "$f" "$h"
  done

  echo
  echo "### verdict per field across the four containers"
  for jf in $FIELDS; do
    a="$(jget "$RAW/20_probe_arch.json" "$jf")"
    u="$(jget "$RAW/20_probe_ubuntu2404.json" "$jf")"
    d="$(jget "$RAW/20_probe_debian13.json" "$jf")"
    f="$(jget "$RAW/20_probe_fedora42.json" "$jf")"
    if [ "$a" = "$u" ] && [ "$a" = "$d" ] && [ "$a" = "$f" ]; then
      printf 'SAME     %-30s = %s\n' "${jf#.}" "$a"
    else
      printf 'DIFFERS  %-30s arch=%s ubuntu=%s debian=%s fedora=%s\n' \
        "${jf#.}" "$a" "$u" "$d" "$f"
    fi
  done

  echo
  echo "### kernel-derived fields: container must equal host (same kernel)"
  for jf in '.host.uname_r' '.procfs.self_stat_fields' '.procfs.pid_max' \
            '.inotify_limits.max_user_watches' \
            '.inotify_limits.max_user_instances' \
            '.inotify_limits.max_queued_events'; do
    h="$(jget "$RAW/01_probe_host_bash.json" "$jf")"
    a="$(jget "$RAW/20_probe_arch.json" "$jf")"
    if [ "$h" = "$a" ]; then
      printf 'SAME     %-30s host=%s container=%s\n' "${jf#.}" "$h" "$a"
    else
      printf 'DIFFERS  %-30s host=%s container=%s\n' "${jf#.}" "$h" "$a"
    fi
  done

  echo
  echo "### userland-derived fields: container MUST differ from host"
  for jf in '.init.pid1_comm' '.init.pid1_is_systemd' '.os_release.ID' \
            '.shells.bin_sh_impl'; do
    h="$(jget "$RAW/01_probe_host_bash.json" "$jf")"
    a="$(jget "$RAW/20_probe_arch.json" "$jf")"
    printf 'CONTRAST %-30s host=%s container=%s\n' "${jf#.}" "$h" "$a"
  done
} >"$RAW/30_matrix.log" 2>&1
echo "-> 30_matrix.log"
tail -n "$TAIL_LINES" "$RAW/30_matrix.log"

# ---------------------------------------------------------------------------
# Stage 5 — binary distribution summary
# ---------------------------------------------------------------------------
echo
echo "=== stage 5: binary distribution summary"

HOST_GLIBC="$(ldd --version 2>/dev/null | head -1 | awk '{print $NF}')"

{
  echo "### Can one binary built on Arch (glibc $HOST_GLIBC) run everywhere?"
  echo "### exit codes below are measured, from results/17_distbins_<distro>.log"
  echo
  printf '%-12s %-9s %-12s %-13s %-9s\n' \
    DISTRO container_glibc hello_dyn hello_static hello_newer
  for image in "${IMAGES[@]}"; do
    label="$(label_for "$image")"
    f="$RAW/17_distbins_${label}.log"
    [ -f "$f" ] || continue
    dyn="$(sed -n 's/^hello_dyn exit = //p' "$f" | head -1)"
    sta="$(sed -n 's/^hello_static exit = //p' "$f" | head -1)"
    new="$(sed -n 's/^hello_newer exit = //p' "$f" | head -1)"
    lic="$(sed -n 's/^ldd .* \([0-9][0-9.]*\)$/\1/p' "$f" | head -1)"
    [ -n "$lic" ] || lic="$(jget "$RAW/20_probe_${label}.json" '.libc.glibc_version')"
    printf '%-12s %-9s %-12s %-13s %-9s\n' \
      "$label" "${lic:--}" "${dyn:--}" "${sta:--}" "${new:--}"
  done
  echo
  echo "### 0 = ran fine, non-zero = failed"
  echo
  echo "### loader error messages, verbatim:"
  grep -h -E 'not found \(required|error while loading|cannot execute' \
    "$RAW"/17_distbins_*.log 2>/dev/null | sed 's|/probe-bin/||g' |
    sort -u | sed 's/^/  /'
  echo
  echo "### GLIBC version nodes each artefact requires (host-side, from build log):"
  sed -n '/GLIBC version nodes required/,$p' "$RAW/00_build_artefacts.log" |
    grep -E '^--- (hello|capcheck)' -A3 | head -30
} >"$RAW/31_dist_matrix.log" 2>&1
echo "-> 31_dist_matrix.log"
tail -n "$TAIL_LINES" "$RAW/31_dist_matrix.log"

# ---------------------------------------------------------------------------
# Stage 6 — init-detection evidence table
# ---------------------------------------------------------------------------
echo
echo "=== stage 6: init detection evidence"

{
  echo "### Does the presence of the systemctl binary indicate a running systemd?"
  echo "### Method A = test -x systemctl ; Method B = systemctl is-system-running"
  echo "### Method C = /run/systemd/system exists ; Method D = /proc/1/comm"
  echo
  printf '%-12s %-11s %-16s %-9s %-11s %-7s\n' \
    DISTRO methodA methodB methodC methodD correct
  for image in "${IMAGES[@]}"; do
    label="$(label_for "$image")"
    f="$RAW/16_initdetect_${label}.log"
    [ -f "$f" ] || continue

    if grep -q 'systemctl: PRESENT' "$f"; then a=PRESENT; else a=ABSENT; fi

    b="$(sed -n 's/^is-system-running -> //p' "$f" | head -1)"
    if [ -z "$b" ]; then
      if grep -q 'skipped (no systemctl binary)' "$f"; then b="n/a"; else b="error"; fi
    fi

    if grep -q "^ls: cannot access '/run/systemd/system'" "$f"; then
      c=ABSENT
    elif grep -q '/run/systemd/system' "$f"; then
      c=PRESENT
    else
      c="?"
    fi

    d="$(sed -n 's/^pid1 comm:  *//p' "$f" | head -1)"
    v="$(sed -n 's/^systemd_running = \(true\|false\).*/\1/p' "$f" | head -1)"
    printf '%-12s %-11s %-16s %-9s %-11s %-7s\n' \
      "$label" "$a" "$b" "$c" "${d:--}" "${v:--}"
  done
  echo
  echo "### truth: no container runs systemd as PID 1; PID 1 is the probe shell"
  echo "### therefore methodA (binary) is a false positive wherever the package is installed,"
  echo "### methodB is 'offline' (a real systemd answer) even though systemd is NOT init,"
  echo "### methodC is a side effect of the image, and methodD is the only sound test."
  echo
  echo "### host contrast (systemd really is init there), from 01_probe_host_bash.json:"
  printf 'pid1_comm=%s pid1_is_systemd=%s systemctl=%s is_system_running=%s\n' \
    "$(jget "$RAW/01_probe_host_bash.json" '.init.pid1_comm')" \
    "$(jget "$RAW/01_probe_host_bash.json" '.init.pid1_is_systemd')" \
    "$(jget "$RAW/01_probe_host_bash.json" '.init.systemctl_present')" \
    "$(jget "$RAW/01_probe_host_bash.json" '.init.systemctl_is_system_running')"
} >"$RAW/32_initdetect_matrix.log" 2>&1
echo "-> 32_initdetect_matrix.log"
tail -n "$TAIL_LINES" "$RAW/32_initdetect_matrix.log"

# ---------------------------------------------------------------------------
# Stage 7 — shell-difference evidence table
# ---------------------------------------------------------------------------
echo
echo "=== stage 7: /bin/sh difference evidence"

{
  echo "### Same commands, different /bin/sh. Raw output in results/11_shell_*.log"
  echo
  printf '%-12s %-9s %-15s %-16s %-13s %-11s\n' \
    DISTRO bin_sh BASH_VERSION local echo_-e pipefail
  for image in "${IMAGES[@]}"; do
    label="$(label_for "$image")"
    f="$RAW/11_shell_${label}.log"
    [ -f "$f" ] || continue
    sh="$(jget "$RAW/20_probe_${label}.json" '.shells.bin_sh_impl')"
    bv="$(grep -m1 -oE 'BASH_VERSION_SET=[^ ]*|BASH_VERSION_EMPTY' "$f")"
    lc="$(grep -m1 -oE 'LOCAL_OK|local: not found|local: [a-z]+' "$f")"
    ee="$(sed -n '/TEST C/,/TEST D/p' "$f" |
      grep -m1 -oE 'a \\t b|a\tb|a \\\\ t b' || echo '?')"
    pf="$(grep -m1 -oE 'PIPEFAIL_SUPPORTED|PIPEFAIL_UNSUPPORTED' "$f")"
    printf '%-12s %-9s %-15s %-16s %-13s %-11s\n' \
      "$label" "$sh" "${bv:--}" "${lc:--}" "${ee:--}" \
      "${pf#PIPEFAIL_}"
  done
  echo
  echo "### raw echo -e output, byte-exact (od -c), per distro:"
  for image in "${IMAGES[@]}"; do
    label="$(label_for "$image")"
    echo "--- $label"
    sed -n '/TEST C/,/TEST D/p' "$RAW/11_shell_${label}.log" |
      grep -vE '^--- TEST|^$'
  done
} >"$RAW/33_shell_matrix.log" 2>&1
echo "-> 33_shell_matrix.log"
tail -n "$TAIL_LINES" "$RAW/33_shell_matrix.log"

# ---------------------------------------------------------------------------
# Stage 8 — results index
# ---------------------------------------------------------------------------
echo
echo "=== stage 8: results index"
{
  echo "# results index ($STAMP)"
  echo
  printf '%-34s %10s\n' FILE BYTES
  for f in "$RAW"/*.log "$RAW"/*.json; do
    [ -f "$f" ] || continue
    printf '%-34s %10s\n' "$(basename "$f")" "$(stat -c %s "$f")"
  done
  echo
  echo "total files: $(find "$RAW" -maxdepth 1 -type f | wc -l)"
} >"$RAW/99_index.log" 2>&1
tail -n "$TAIL_LINES" "$RAW/99_index.log"

echo
echo "=== 0-F driver complete"
