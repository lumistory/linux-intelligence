#!/usr/bin/env bash
# Phase 0 / experiment 0-E driver.
#
# Reproduces every number in REPORT.md. All writes go to this experiment's
# directory and /tmp/li-phase0-0e-*; nothing is run as root; no sudo.
#
# Usage: ./run.sh [--quick]
#   --quick   smaller iteration counts (structure check, not report data)
#
# Exits non-zero on failure.
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
mkdir -p "$RAW"

# All scratch space is under /tmp/li-phase0-0e-<something>.
SCRATCH_ROOT="/tmp/li-phase0-0e-scratch"

if [ "$QUICK" -eq 1 ]; then
  SPAWN_N=100; POLL_60S=8000; FILES_OVERFLOW=20000; FORKS_WRAP=400
else
  SPAWN_N=500; POLL_60S=60000; FILES_OVERFLOW=50000; FORKS_WRAP=900
fi

log() { printf '=== %s ===\n' "$*" >&2; }

cleanup_scratch() {
  # Delete only our own scratch trees, one level deep, never a glob that could
  # escape. Uses find -delete which refuses to cross mount points by default.
  for d in "$SCRATCH_ROOT" /tmp/li-phase0-0e-fs /tmp/li-phase0-0e-rec /tmp/li-phase0-0e-ovf; do
    if [ -d "$d" ] && [ "${d#/tmp/li-phase0-0e}" != "$d" ]; then
      find "$d" -mindepth 0 -delete 2>/dev/null || true
    fi
  done
}

# Prepare a clean scratch tree. mkdir -p is idempotent; stale contents would
# change directory counts, so they are removed first via find -delete.
fresh_dir() {
  local d="$1"
  mkdir -p "$d"
  find "$d" -mindepth 1 -delete 2>/dev/null || true
}

detect_env() {
  local out="$RAW/env-$STAMP.log"
  {
    echo "# host"
    uname -srm
    echo "# os"
    (cat /etc/os-release 2>/dev/null | grep -E '^(PRETTY_NAME|VERSION_ID)=') || true
    echo "# rust"
    rustc --version 2>&1 || true
    cargo --version 2>&1 || true
    echo "# nproc / mem"
    nproc
    (awk '/MemTotal/{print $2" kB"}' /proc/meminfo) || true
    echo "# inotify limits"
    for f in max_user_watches max_queued_events max_user_instances; do
      printf '%s=%s\n' "$f" "$(cat "/proc/sys/fs/inotify/$f" 2>&1)"
    done
    echo "# pid"
    printf 'pid_max=%s\n' "$(cat /proc/sys/kernel/pid_max)"
    printf 'clk_tck=%s\n' "$(getconf CLK_TCK)"
    printf 'btime=%s\n' "$(awk '/btime/{print $2}' /proc/stat)"
    echo "# proc_events"
    if [ -r /proc/config.gz ]; then
      printf 'config_source=/proc/config.gz\n'
      (zgrep -E '^CONFIG_PROC_EVENTS|^CONFIG_CONNECTOR' /proc/config.gz) || echo 'CONFIG_PROC_EVENTS not found'
    elif ls /boot/config-* >/dev/null 2>&1; then
      printf 'config_source=/boot/config-%s\n' "$(uname -r)"
      grep -E '^CONFIG_PROC_EVENTS|^CONFIG_CONNECTOR' /boot/config-"$(uname -r)" || echo 'CONFIG_PROC_EVENTS not found'
    else
      echo 'config_source=NOT AVAILABLE (no /proc/config.gz and no /boot/config-*)'
    fi
    echo "# proc mount"
    grep -E ' /proc ' /proc/self/mountinfo || true
    echo "# user"
    id
    echo "# scratch fs"
    df -hT /tmp | cat
    echo "# toolchain deps (offline)"
    cargo tree --offline --depth 1 2>&1 || true
  } 2>&1 | tee "$out"
}

build() {
  log "build"
  cargo build --offline --release 2>&1 | tee "$RAW/build-$STAMP.log"
}

# ---------------------------------------------------------------------------
# A. Process observer
# ---------------------------------------------------------------------------

proc_poll_loss() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A1-poll-loss-$STAMP.log"
  local rate=20
  : >"$out"
  {
    echo "# 500 /bin/true spawns per interval, observed at 50/200/1000 ms."
    echo "# Ground truth = the (pid,starttime) key read by the launcher itself, so"
    echo "# the denominator is exact and is not itself a sample."
    echo "# Spawns are jittered: without jitter a spawn period that divides the"
    echo "# poll interval phase-locks and the loss figure becomes an aliasing"
    echo "# artifact (observed: exactly 0/25/50/100% rather than a curve)."
    echo "# A window equal to the interval is used so the scan cost (2-4 ms) does"
    echo "# not itself consume a large fraction of the sampling opportunity."
  } >>"$out"

  # Primary table: interval x short-lived process. life_ms=0 is /bin/true.
  for iv in 50 200 1000; do
    # The binary extends the window itself so the whole spawn series fits inside
    # it; the value below is only a floor.
    local dur=$((iv * 6))
    [ "$dur" -lt 6000 ] && dur=6000
    if [ "$QUICK" -eq 1 ]; then dur=$((iv * 3)); [ "$dur" -lt 3000 ] && dur=3000; fi
    "$bin" poll --interval-ms "$iv" --duration-ms "$dur" --rate "$rate" --jitter \
      --spawn "$SPAWN_N" --life-ms 0 --depth stat --label "iv${iv}-true" 2>&1 | tee -a "$out"
    echo >>"$out"
  done

  # Control: identical spawning, but children are never reaped, so they stay in
  # /proc as zombies. This isolates "the kernel hides the process" (it does not)
  # from "the process was gone before the next scan" (it was).
  {
    echo "## Control: same spawn pattern, children left as zombies (--no-reap)"
    echo "# If loss falls sharply here, the mechanism is process lifetime, not"
    echo "# /proc visibility."
  } >>"$out"
  local dur=8000
  if [ "$QUICK" -eq 1 ]; then dur=4000; fi
  "$bin" poll --interval-ms 200 --duration-ms "$dur" --rate "$rate" --jitter \
    --spawn "$SPAWN_N" --life-ms 0 --no-reap --depth stat --label "iv200-zombie" 2>&1 | tee -a "$out"
  echo >>"$out"
}

# The lifetime threshold: loss as a function of how long the process lives,
# against the theoretical floor 1 - (life + scan_time)/interval.
proc_life_threshold() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A1b-life-threshold-$STAMP.log"
  local rate=20
  : >"$out"
  {
    echo "# loss vs process lifetime at a fixed 200 ms poll interval."
    echo "# Prediction: a scan samples one instant per interval, so a process of"
    echo "# lifetime L is caught with probability ~(L + scan_time)/interval, and"
    echo "# loss ~ 1 - (L + scan_time)/interval."
    echo "# rate=$rate/s with jitter; 120 spawns per row."
  } >>"$out"
  local lives="0 5 15 30 60 120 250"
  if [ "$QUICK" -eq 1 ]; then lives="0 30 250"; fi
  for life in $lives; do
    "$bin" poll --interval-ms 200 --duration-ms 9000 --rate "$rate" --jitter \
      --spawn 120 --life-ms "$life" --depth stat --label "life${life}ms" 2>&1 | tee -a "$out"
    echo >>"$out"
  done
}

proc_poll_depth() {
  # Same measurement at one interval with the full read (stat+status+cmdline),
  # so the cost of the richer scan is visible.
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A2-poll-depth-$STAMP.log"
  "$bin" poll --interval-ms 200 --duration-ms 8000 --spawn "$SPAWN_N" \
    --depth full --label "full-depth-200ms" 2>&1 | tee "$out"
}

proc_scan_latency() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A3-scan-latency-$STAMP.log"
  {
    echo "# repeated single-scan latency, both depths"
    "$bin" scan-once --iters 200 --depth stat
    echo
    "$bin" scan-once --iters 200 --depth full
  } 2>&1 | tee "$out"
}

proc_overhead_60s() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A4-overhead-60s-$STAMP.log"
  local dur=60000
  if [ "$QUICK" -eq 1 ]; then dur=8000; fi
  "$bin" poll --interval-ms 200 --duration-ms "$dur" --depth full --label "overhead-60s" \
    2>&1 | tee "$out"
}

proc_pidfd() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A5-pidfd-$STAMP.log"
  "$bin" pidfd --spawn 50 --hold-ms 300 2>&1 | tee "$out"
}

proc_reuse() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A6-pid-reuse-$STAMP.log"
  : >"$out"
  {
    echo "## (a) forced reuse via ns_last_pid, distinct ticks"
    echo "# unshare -Urpf gives a private pid namespace where ns_last_pid is writable."
  } >>"$out"
  unshare -Urpf --mount-proc "$bin" reuse --target-pid 4242 2>&1 | tee -a "$out"
  {
    echo
    echo "## (b) forced reuse inside one clock tick (adversarial for starttime)"
    echo "# getconf CLK_TCK = 100, so starttime has 10 ms granularity. Two"
    echo "# processes created in the same tick at the same pid share BOTH pid and"
    echo "# starttime, which falsifies (pid,starttime) as a unique key."
  } >>"$out"
  local hits=0
  local trials=20
  for i in $(seq 1 "$trials"); do
    local r
    r=$(unshare -Urpf --mount-proc "$bin" reuse --target-pid $((6000 + i)) --mode same-tick 2>/dev/null \
      | grep -E 'pid_starttime_key_collides' || true)
    echo "trial$i $r" >>"$out"
    case "$r" in *true*) hits=$((hits + 1)) ;; esac
  done
  printf 'same_tick_collision_trials %d\nsame_tick_collisions %d\n' "$trials" "$hits" | tee -a "$out"

  {
    echo
    echo "## (c) natural pid-space wraparound (small pid_max in a private pidns)"
    echo "# pid_max is writable inside the namespace and does not affect the host."
  } >>"$out"
  unshare -Urpf --mount-proc "$bin" wrap --pid-max 301 --forks "$FORKS_WRAP" 2>&1 \
    | grep -vE '^\s+pid [0-9]+: \[' | tee -a "$out"
  {
    echo
    echo "## (d) host pid_max after namespaced writes (must be unchanged)"
    cat /proc/sys/kernel/pid_max
  } >>"$out"
}

proc_orphan() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A7-orphan-$STAMP.log"
  "$bin" orphan 2>&1 | tee "$out"
}

proc_readability() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A8-readability-$STAMP.log"
  {
    "$bin" readability
    echo
    echo "## hidepid mount attempt (unprivileged)"
    echo "# Try to mount a second procfs with hidepid=2 inside a private mount ns."
    mnt=/tmp/li-phase0-0e-hidepid
    mkdir -p "$mnt"
    if unshare -Urpf sh -c "mount -t proc -o hidepid=2 proc '$mnt' 2>&1"; then
      echo "hidepid_mount=OK"
      echo "visible_entries=$(ls "$mnt" | grep -c '^[0-9]' || true)"
    else
      echo "hidepid_mount=FAILED (see errno above)"
      echo "hidepid_status=NOT TESTED"
    fi
  } 2>&1 | tee "$out"
}

proc_connector() {
  local bin="$EXP_DIR/target/release/proc_observer"
  local out="$RAW/A9-connector-$STAMP.log"
  {
    echo "## host network namespace"
    "$bin" connector --spawn 500
    echo
    echo "## fresh network namespace (unshare -Urn): no connector registered"
    unshare -Urn "$bin" connector --spawn 10 2>&1 || echo "exit=$?"
  } 2>&1 | tee "$out"
}

# ---------------------------------------------------------------------------
# B. Filesystem observer
# ---------------------------------------------------------------------------

fs_event_sequences() {
  local bin="$EXP_DIR/target/release/fs_observer"
  local root="$SCRATCH_ROOT/seq"
  local out="$RAW/B1-event-sequences-$STAMP.log"
  fresh_dir "$root"
  {
    echo "# watch root: $root"
    echo "# All operations below are issued from the shell while the watcher runs."
  } >"$out"
  "$bin" watch --root "$root" --seconds 14 >"$RAW/.b1-watch-$STAMP.tmp" 2>&1 &
  local wpid=$!
  sleep 1
  (
    cd "$root"
    echo "## create" >>"$out"
    echo hello > created.txt
    sleep 0.2
    echo "## modify (append)" >>"$out"
    echo world >> created.txt
    sleep 0.2
    echo "## chmod" >>"$out"
    chmod 600 created.txt
    sleep 0.2
    echo "## mv same directory" >>"$out"
    mv created.txt renamed.txt
    sleep 0.2
    echo "## mkdir" >>"$out"
    mkdir sub
    sleep 0.3
    echo "## create inside sub (watched only if recursive)" >>"$out"
    echo nested > sub/nested.txt
    sleep 0.2
    echo "## mv across directories within root" >>"$out"
    mv renamed.txt sub/
    sleep 0.2
    echo "## hard link" >>"$out"
    ln sub/nested.txt hardlink.txt
    sleep 0.2
    echo "## symlink" >>"$out"
    ln -s sub/nested.txt symlink.txt
    sleep 0.2
    echo "## unlink" >>"$out"
    unlink hardlink.txt
    sleep 0.2
    echo "## rm -f within scratch root" >>"$out"
    unlink symlink.txt
    sleep 0.3
  ) >>"$out" 2>&1
  wait $wpid || true
  {
    echo
    echo "## watcher event log (non-recursive: sub/ events are NOT expected)"
  } >>"$out"
  cat "$RAW/.b1-watch-$STAMP.tmp" >>"$out"
  rm -f "$RAW/.b1-watch-$STAMP.tmp"
}

fs_cross_fs_move() {
  local bin="$EXP_DIR/target/release/fs_observer"
  # A genuine cross-device move: /tmp is tmpfs, the experiment directory is
  # btrfs. Renaming across devices cannot preserve the inode, so the kernel must
  # degrade the operation to copy+unlink, which produces a different event
  # sequence than an intra-filesystem rename.
  local root="$EXP_DIR/results/xfs-target"
  local src="$SCRATCH_ROOT/xfs-src"
  local out="$RAW/B2-crossfs-move-$STAMP.log"
  fresh_dir "$root"
  fresh_dir "$src"
  {
    echo "# src=$src  fstype=$(df -T "$src" | awk 'NR==2{print $2}')  dev=$(df -T "$src" | awk 'NR==2{print $1}')"
    echo "# dst=$root  fstype=$(df -T "$root" | awk 'NR==2{print $2}')  dev=$(df -T "$root" | awk 'NR==2{print $1}')"
    echo "# Same-device rename for contrast is covered in B1 (mv within one dir)."
  } >"$out"
  "$bin" watch --root "$root" --seconds 6 >"$RAW/.b2-watch-$STAMP.tmp" 2>&1 &
  local wpid=$!
  sleep 1
  printf 'payload\n' >"$src/moving.txt"
  sync
  mv "$src/moving.txt" "$root/moving.txt" 2>>"$out" || echo "mv failed" >>"$out"
  sleep 3
  wait $wpid || true
  {
    echo
    echo "## events for the cross-device move"
    cat "$RAW/.b2-watch-$STAMP.tmp"
    echo
    printf 'move_pairs_seen %s (0 = no cookie pair, i.e. not a rename)\n' \
      "$(grep -c ' MOVED_FROM ' "$RAW/.b2-watch-$STAMP.tmp" || true)"
  } >>"$out"
  rm -f "$RAW/.b2-watch-$STAMP.tmp"
  find "$root" -mindepth 1 -delete 2>/dev/null || true
  rmdir "$root" 2>/dev/null || true
}

fs_large_write() {
  local bin="$EXP_DIR/target/release/fs_observer"
  local root="$SCRATCH_ROOT/big"
  local out="$RAW/B3-large-write-$STAMP.log"
  fresh_dir "$root"
  "$bin" watch --root "$root" --seconds 8 >"$RAW/.b3-watch-$STAMP.tmp" 2>&1 &
  local wpid=$!
  sleep 1
  # One 100 MB write() from a single dd process, then a second run via
  # std::fs::write to show the count depends on the writer's buffer strategy.
  {
    echo "# 100MB via dd bs=100M count=1 (single write syscall)"
  } >"$out"
  dd if=/dev/zero of="$root/one-write.bin" bs=100M count=1 2>>"$out" || true
  sleep 0.5
  {
    echo "# 100MB via dd bs=1M count=100 (100 write syscalls)"
  } >>"$out"
  dd if=/dev/zero of="$root/many-writes.bin" bs=1M count=100 2>>"$out" || true
  sleep 3
  wait $wpid || true
  {
    echo
    echo "## events"
    cat "$RAW/.b3-watch-$STAMP.tmp"
    echo
    echo "## MODIFY counts per file"
    # Count only real event rows: they begin with the monotonic timestamp and
    # carry the mask name as a standalone field. Anchoring on the row shape
    # avoids matching the summary lines that this very block writes.
    printf 'one-write.bin  MODIFY=%s\n' "$(awk '$1 ~ /^[0-9]+$/ && $5 == "MODIFY" && $9 == "one-write.bin"' "$RAW/.b3-watch-$STAMP.tmp" | wc -l)"
    printf 'many-writes.bin MODIFY=%s\n' "$(awk '$1 ~ /^[0-9]+$/ && $5 == "MODIFY" && $9 == "many-writes.bin"' "$RAW/.b3-watch-$STAMP.tmp" | wc -l)"
    printf 'one-write.bin  CLOSE_WRITE=%s\n' "$(awk '$1 ~ /^[0-9]+$/ && $5 == "CLOSE_WRITE" && $9 == "one-write.bin"' "$RAW/.b3-watch-$STAMP.tmp" | wc -l)"
    printf 'many-writes.bin CLOSE_WRITE=%s\n' "$(awk '$1 ~ /^[0-9]+$/ && $5 == "CLOSE_WRITE" && $9 == "many-writes.bin"' "$RAW/.b3-watch-$STAMP.tmp" | wc -l)"
  } >>"$out"
  rm -f "$RAW/.b3-watch-$STAMP.tmp"
  rm -f "$root/one-write.bin" "$root/many-writes.bin"
}

fs_recursion() {
  local bin="$EXP_DIR/target/release/fs_observer"
  local out="$RAW/B4-recursion-$STAMP.log"
  # Case 1: non-recursive watch, files created in a new subdirectory -> lost.
  local r1="$SCRATCH_ROOT/rec-noauto"
  fresh_dir "$r1"
  "$bin" watch --root "$r1" --seconds 6 >"$RAW/.b4a-$STAMP.tmp" 2>&1 &
  local w1=$!
  sleep 1
  mkdir -p "$r1/newdir"
  sleep 0.2
  for i in 1 2 3; do printf 'x\n' >"$r1/newdir/file$i.txt"; done
  sleep 0.2
  for i in 4 5; do printf 'x\n' >"$r1/top$i.txt"; done
  sleep 2
  wait $w1 || true

  # Case 2: recursive at start (existing dirs), then a NEW subdir appears and
  # the watcher auto-adds it. Pre-existing nested content is visible, events in
  # the mkdir->add_watch gap are not.
  local r2="$SCRATCH_ROOT/rec-auto"
  fresh_dir "$r2"
  mkdir -p "$r2/existing/inner"
  "$bin" watch --root "$r2" --recursive --auto-add --seconds 6 >"$RAW/.b4b-$STAMP.tmp" 2>&1 &
  local w2=$!
  sleep 1
  printf 'x\n' >"$r2/existing/inner/preexisting-target.txt"
  mkdir -p "$r2/brand-new"
  # Race the gap deliberately: create a file immediately after mkdir.
  printf 'x\n' >"$r2/brand-new/immediate.txt"
  sleep 0.5
  printf 'x\n' >"$r2/brand-new/after-gap.txt"
  sleep 2
  wait $w2 || true

  {
    echo "## Case 1: single non-recursive watch, events in a new subdirectory"
    echo "# expected: CREATE for newdir and top4/top5, NOTHING for newdir/file*.txt"
    cat "$RAW/.b4a-$STAMP.tmp"
    echo
    printf 'case1_events_in_newdir %s\n' "$(grep -c 'newdir/file' "$RAW/.b4a-$STAMP.tmp" || true)"
    printf 'case1_events_at_top    %s\n' "$(grep -cE 'top[0-9]\.txt' "$RAW/.b4a-$STAMP.tmp" || true)"
    echo
    echo "## Case 2: recursive + auto-add on new directories"
    echo "# expected: preexisting-target.txt seen; immediate.txt may be missed (gap)"
    cat "$RAW/.b4b-$STAMP.tmp"
    echo
    printf 'case2_preexisting_seen %s\n' "$(grep -c 'preexisting-target' "$RAW/.b4b-$STAMP.tmp" || true)"
    printf 'case2_immediate_seen   %s\n' "$(grep -c 'immediate.txt' "$RAW/.b4b-$STAMP.tmp" || true)"
    printf 'case2_after_gap_seen   %s\n' "$(grep -c 'after-gap.txt' "$RAW/.b4b-$STAMP.tmp" || true)"
  } >"$out"
  rm -f "$RAW/.b4a-$STAMP.tmp" "$RAW/.b4b-$STAMP.tmp"

  # Watch count vs directory count.
  local r3="$SCRATCH_ROOT/watchcount"
  fresh_dir "$r3"
  local i
  for i in $(seq 1 10); do
    mkdir -p "$r3/d$i"
    local j
    for j in $(seq 1 5); do mkdir -p "$r3/d$i/s$j"; done
  done
  "$bin" watch-count --root "$r3" 2>&1 | tee -a "$out"
  printf '# 10 top dirs x 5 subdirs = 50 subdirs + 10 parents = 60 dirs, so 61 watches\n' >>"$out"
}

fs_overflow() {
  local bin="$EXP_DIR/target/release/fs_observer"
  local out="$RAW/B5-overflow-$STAMP.log"
  local r1="$SCRATCH_ROOT/ovf-fast"
  local r2="$SCRATCH_ROOT/ovf-slow"
  fresh_dir "$r1"
  fresh_dir "$r2"
  {
    echo "## fast consumer (drains continuously)"
    "$bin" overflow --root "$r1" --files "$FILES_OVERFLOW" --buffer 65536
    echo
    echo "## slow consumer (300 ms stall before first drain)"
    "$bin" overflow --root "$r2" --files "$FILES_OVERFLOW" --buffer 65536 --slow-ms 300
  } 2>&1 | tee "$out"
  find "$r1" -mindepth 1 -delete 2>/dev/null || true
  find "$r2" -mindepth 1 -delete 2>/dev/null || true
}

fs_permissions() {
  local bin="$EXP_DIR/target/release/fs_observer"
  local out="$RAW/B6-permissions-$STAMP.log"
  {
    echo "# watch a directory owned by another uid (root, mode 0700)"
    set +e
    "$bin" watch --root /root --seconds 1 2>&1
    echo "exit_code=$?"
    echo
    echo "# watch another root-only path"
    "$bin" watch --root /var/lib/private --seconds 1 2>&1
    echo "exit_code=$?"
    echo
    echo "# watch a nonexistent path (for contrast: ENOENT, not EACCES)"
    "$bin" watch --root /tmp/li-phase0-0e-does-not-exist --seconds 1 2>&1
    echo "exit_code=$?"
    echo
    echo "# watch a regular file (no IN_ONLYDIR in our mask: this is ALLOWED,"
    echo "# and is the documented way to watch a single file's IN_MODIFY)"
    echo x >/tmp/li-phase0-0e-regular-file
    "$bin" watch --root /tmp/li-phase0-0e-regular-file --seconds 2 2>&1 &
    local fp=$!
    sleep 0.5
    echo appended >>/tmp/li-phase0-0e-regular-file
    wait $fp 2>/dev/null || true
    echo "exit_code=$?"
    echo
    echo "# for contrast, the same path with IN_ONLYDIR would fail ENOTDIR; our"
    echo "# default mask does not set it, so the success above is expected"
    set -e
  } 2>&1 | tee "$out"
}

fs_editor_save() {
  local bin="$EXP_DIR/target/release/fs_observer"
  local root="$SCRATCH_ROOT/editor"
  local out="$RAW/B7-editor-save-$STAMP.log"
  fresh_dir "$root"
  printf 'original\n' >"$root/doc.txt"
  "$bin" watch --root "$root" --seconds 10 >"$RAW/.b7-watch-$STAMP.tmp" 2>&1 &
  local wpid=$!
  sleep 1
  (
    cd "$root"
    echo "## vim ex-mode :wq (writes in place by default: backupcopy=auto)"
    vim -E -s -c 'call setline(1,"edited-by-vim")' -c 'wq' doc.txt 2>&1 || true
    sleep 1
    echo "## vim with backupcopy=no (temp file + rename, the common modern default"
    echo "## for many editors)"
    vim -E -s -c 'set backupcopy=no' -c 'call setline(1,"second-edit")' -c 'wq' doc.txt 2>&1 || true
    sleep 0.5
    echo "## nano (temp file + rename)"
    printf 'nano-will-edit\n' >nano.txt
    nano --saveonexit --tempfile nano.txt </dev/null >/dev/null 2>&1 || true
    sleep 0.5
    echo "## explicit atomic-replace pattern (what editors do)"
    printf 'atomic-new\n' >.doc.txt.tmp
    mv -f .doc.txt.tmp doc.txt
    sleep 0.5
  ) >>"$out" 2>&1
  wait $wpid || true
  {
    echo
    echo "## events (note MOVED_FROM+MOVED_TO sharing a cookie = rename/atomic replace)"
    cat "$RAW/.b7-watch-$STAMP.tmp"
    echo
    printf 'create_count %s\n' "$(grep -c ' CREATE ' "$RAW/.b7-watch-$STAMP.tmp" || true)"
    printf 'move_pairs   %s\n' "$(grep -c ' MOVED_FROM ' "$RAW/.b7-watch-$STAMP.tmp" || true)"
  } >>"$out"
  rm -f "$RAW/.b7-watch-$STAMP.tmp"
}

fs_attribution() {
  local bin="$EXP_DIR/target/release/fs_observer"
  local root="$SCRATCH_ROOT/attrib"
  local out="$RAW/B8-attribution-$STAMP.log"
  fresh_dir "$root"
  "$bin" watch --root "$root" --seconds 8 >"$RAW/.b8-watch-$STAMP.tmp" 2>&1 &
  local wpid=$!
  sleep 1
  {
    echo "# Two distinct processes write to two files in the same watched directory."
    echo "# Each writer prints its own pid; the event log has no pid at all."
    echo "# Writers are separate /bin/sh processes (not subshells of this script),"
    echo "# so the pids printed below are genuinely different processes."
  } >"$out"
  # Each writer is a separate /bin/sh process; `$$` inside it is that process.
  local pa pb pc pd
  /bin/sh -c "echo writer_A pid=\$\$; printf 'from-A\n' >'$root/from-a.txt'; sleep 0.3" &
  pa=$!
  /bin/sh -c "echo writer_B pid=\$\$; printf 'from-B\n' >'$root/from-b.txt'; sleep 0.3" &
  pb=$!
  wait $pa $pb || true
  # Same file, two writers: events interleave with no attribution.
  /bin/sh -c "echo writer_C pid=\$\$ '(same file as D)'; i=1; while [ \$i -le 3 ]; do printf 'C%s\n' \$i >>'$root/shared.txt'; sleep 0.1; i=\$((i+1)); done" &
  pc=$!
  /bin/sh -c "echo writer_D pid=\$\$ '(same file as C)'; i=1; while [ \$i -le 3 ]; do printf 'D%s\n' \$i >>'$root/shared.txt'; sleep 0.1; i=\$((i+1)); done" &
  pd=$!
  wait $pc $pd || true
  sleep 2
  wait $wpid || true
  {
    echo
    echo "## events"
    cat "$RAW/.b8-watch-$STAMP.tmp"
    echo
    echo "## fields present in every event: wd, mask, cookie, name. No pid, no uid."
    echo "## stat check: the pids above are distinct processes writing distinct files"
    for f in from-a.txt from-b.txt; do
      printf '%s size=%s\n' "$f" "$(stat -c %s "$root/$f" 2>/dev/null || echo missing)"
    done
    echo "# shared.txt events are indistinguishable by writer; only the count and"
    echo "# the interleaving are visible."
  } >>"$out"
  rm -f "$RAW/.b8-watch-$STAMP.tmp"
}

fs_cost() {
  local bin="$EXP_DIR/target/release/fs_observer"
  local root="$SCRATCH_ROOT/cost"
  local out="$RAW/B9-cost-$STAMP.log"
  fresh_dir "$root"
  "$bin" cost --root "$root" --files 1000 2>&1 | tee "$out"
}

# Why PID attribution needs something other than inotify: neither inotify nor
# fanotify can be made to give it to us without privileges here. Both claims are
# measured, not quoted.
fs_attribution_limits() {
  local out="$RAW/B10-attribution-limits-$STAMP.log"
  local src="$SCRATCH_ROOT/fanotify"
  fresh_dir "$src"
  {
    echo "## inotify event ABI has no pid field"
    echo "grep -c pid /usr/include/linux/inotify.h  -> $(grep -c pid /usr/include/linux/inotify.h)"
    echo "# struct inotify_event = { s32 wd; u32 mask; u32 cookie; u32 len; char name[]; }"
    echo
    echo "## fanotify is the API that does carry pid, but it needs privilege"
    cat >"$SCRATCH_ROOT/fanotify_probe.c" <<'EOF'
#include <stdio.h>
#include <fcntl.h>
#include <errno.h>
#include <string.h>
#include <sys/fanotify.h>
int main(int argc, char **argv) {
    const char *path = argv[1];
    int fd = fanotify_init(FAN_CLASS_NOTIF, O_RDONLY | O_LARGEFILE);
    printf("fanotify_init(FAN_CLASS_NOTIF) = %d errno=%d (%s)\n", fd, errno, strerror(errno));
    if (fd < 0) return 1;
    int r = fanotify_mark(fd, FAN_MARK_ADD, FAN_OPEN | FAN_MODIFY, AT_FDCWD, path);
    printf("fanotify_mark = %d errno=%d (%s)\n", r, errno, strerror(errno));
    int fd2 = fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_PIDFD, O_RDONLY | O_LARGEFILE);
    printf("fanotify_init(FAN_REPORT_PIDFD) = %d errno=%d (%s)\n", fd2, errno, strerror(errno));
    return 0;
}
EOF
    if gcc -o "$SCRATCH_ROOT/fanotify_probe" "$SCRATCH_ROOT/fanotify_probe.c" 2>&1; then
      "$SCRATCH_ROOT/fanotify_probe" "$src" 2>&1 || echo "probe_exit=$?"
    else
      echo "compile failed"
    fi
    echo
    echo "## conclusion: unprivileged collection cannot get a pid/writer identity."
    echo "## PID attribution requires eBPF (with CAP_BPF/CAP_PERFMON) or the audit"
    echo "## subsystem (with CAP_AUDIT_WRITE + audit rules installed by root)."
    echo "## Neither is implemented in Phase 0; both are out of scope for 0-E."
  } 2>&1 | tee "$out"
}

# ---------------------------------------------------------------------------
# Summary tables
# ---------------------------------------------------------------------------

summarise() {
  local summary="$RAW/SUMMARY-$STAMP.md"
  {
    echo "# Experiment 0-E measured summary ($STAMP)"
    echo
    echo "## A1. Polling loss on $SPAWN_N /bin/true spawns per row"
    echo
    echo "| interval_ms | life_ms | truth | caught | missed | loss_pct | valid | scans | scan_p50_ns | scan_p99_ns | cpu_total_s |"
    echo "|---|---|---|---|---|---|---|---|---|---|---|"
    awk '
      /^== poll run/ {iv=""; life=""; truth=""; caught=""; missed=""; loss=""; scans=""; p50=""; p99=""; cpu=""; valid=""}
      /^interval_ms/ {iv=$2}
      /^process_life_ms/ {life=$2}
      /^scans / {scans=$2}
      /^scan_latency_ns_p50/ {p50=$2}
      /^scan_latency_ns_p99/ {p99=$2}
      /^cpu_total_s/ {cpu=$2}
      /^truth_pids/ {truth=$2}
      /^caught_pids/ {caught=$2}
      /^missed_pids/ {missed=$2}
      /^loss_pct/ {loss=$2}
      /^run_valid/ {valid=$2; printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n", iv, life, truth, caught, missed, loss, valid, scans, p50, p99, cpu}
    ' "$RAW"/A1-poll-loss-"$STAMP".log
    echo
    echo "Rows are only trustworthy where valid=true; a false marks a window that"
    echo "ended before the spawn series did, so the denominator was not sampled."
    echo
    echo "## A1b. Loss vs process lifetime at 200 ms"
    echo
    echo "| life_ms | caught | truth | loss_pct |"
    echo "|---|---|---|---|"
    awk '
      /^== poll run/ {life=""; caught=""; truth=""; loss=""}
      /^process_life_ms/ {life=$2}
      /^truth_pids/ {truth=$2}
      /^caught_pids/ {caught=$2}
      /^loss_pct/ {loss=$2; printf "| %s | %s | %s | %s |\n", life, caught, truth, loss}
    ' "$RAW"/A1b-life-threshold-"$STAMP".log
    echo
    echo "## A4. 200 ms polling overhead"
    echo
    echo '```'
    grep -E '^(wall_time_s|scans|mean_procs_per_scan|scan_latency_ns_p50|scan_latency_ns_p99|scan_latency_ns_max|cpu_user_s|cpu_system_s|cpu_total_s|cpu_pct_of_one_core|max_rss_kb)' \
      "$RAW/A4-overhead-60s-$STAMP.log" || true
    echo '```'
    echo
    echo "## A9. Proc connector (event-driven route)"
    echo
    echo '```'
    cat "$RAW/A9-connector-$STAMP.log"
    echo '```'
    echo
    echo "## B5. inotify overflow"
    echo
    echo '```'
    grep -E '^(## |files_created|events_received|overflow_events|drain_calls|conclusion|  CREATE|  CLOSE_WRITE|  Q_OVERFLOW)' \
      "$RAW/B5-overflow-$STAMP.log" || true
    echo '```'
    echo
    echo "## B9. inotify observer cost, 1000 creates"
    echo
    echo '```'
    cat "$RAW/B9-cost-$STAMP.log"
    echo '```'
  } >"$summary"
  echo "summary written: $summary" >&2
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

log "experiment 0-E: procfs + inotify ($(date -u +%FT%TZ))"

detect_env
build

# Probes that do not need scratch space.
proc_scan_latency
proc_pidfd
proc_reuse
proc_orphan
proc_readability
proc_connector

# Polling loss (creates children; needs a quiet machine, not scratch space).
proc_poll_loss
proc_life_threshold
proc_poll_depth
proc_overhead_60s

# Filesystem probes.
cleanup_scratch
fs_event_sequences
fs_cross_fs_move
fs_large_write
fs_recursion
fs_overflow
fs_permissions
fs_editor_save
fs_attribution
fs_attribution_limits
fs_cost

summarise
cleanup_scratch

log "done; results in $RAW"
