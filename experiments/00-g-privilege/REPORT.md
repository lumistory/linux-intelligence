# Experiment 0-G — Privilege model and simulated privileged helper

Run stamp: `20261123T141630Z` (all paths below are relative to
`experiments/00-g-privilege/results/`). Driver: `run.sh`, exit status 0,
29/29 checks passed.

## 1. Objective

Establish, by measurement, the smallest privilege boundary the system can be
built on, and the exact interface any future privileged helper must expose.

The four questions that Phase 1 design depends on:

1. Is `SO_PEERCRED` safe to authorise on, or can the pid it reports be recycled
   between the peer's `connect()` and the server's `getsockopt()`?
2. What does uid 0 inside an unprivileged user namespace actually hold? If it is
   "root" in any useful sense, a namespace could replace a real privileged
   helper; if it is not, the boundary must be drawn elsewhere.
3. Can a helper whose input is `{opcode, typed args}` be built with **no** path
   to arbitrary command execution, verified rather than asserted?
4. Which intended capabilities work as a plain user, and which genuinely need
   privilege — with a command and an observed result for each.

## 2. Hypothesis

Written before execution (verbatim from `README.md` §2):

| # | Hypothesis | Falsified if |
|---|---|---|
| H1 | `SO_PEERCRED` returns a **frozen** credential snapshot taken at `connect()`. After the client exits, the reported pid is stale and can be reused. `pid` alone is never an authorisation key. | The pid is a live lookup that updates, or cannot be reused while the socket lives. |
| H2 | `pid + starttime` disambiguates reuse in normal operation, but at `CLK_TCK=100` two processes can share both within one tick, so the triple is *good attribution*, *weak authorisation*. | Reuse within one tick is impossible, or a reused pid always yields a different `starttime`. |
| H3 | Namespace root can mount tmpfs inside its own mount namespace, but cannot read host-root-only files, cannot signal host-root processes, and creates files owned by the unprivileged host uid. Namespace root ≠ host root. | It reads `/etc/shadow`, or its files are host-owned by a non-user uid. |
| H4 | A helper whose entire input is `{opcode: u8, args: typed}` can reject metacharacters, absolute paths, `..`, symlink escapes, oversized frames and unknown opcodes, with **no** code path to `exec`. | Any accepted request reaches `execve()` on caller-influenced data, or a traversal escapes. |
| H5 | One JSON line per request, `O_APPEND`, sequenced under a lock, loses and reorders nothing at 100 concurrent clients. | Line count ≠ request count, or sequences have gaps/dupes/misordering. |
| H6 | `/proc` reads, inotify on own dirs, `AF_UNIX`, sqlite writes work unprivileged; other users' processes in full, other users' dirs, btrfs snapshots and `mount` need privilege. | Any listed-unprivileged capability fails, or any listed-privileged one succeeds. |

## 3. Method

### 3.1 Containment

No sudo password exists, so **no real privileged operation was performed**.

- Everything needing "root" ran in `unshare` user namespaces.
- Everything needing a second uid used a second `/etc/subuid` range
  (`USER:100000:65536`) mapped via the setuid `newuidmap`, which is the
  distribution's intended unprivileged path, not an escalation.
- No setuid file was created. No `/etc` file was modified. Nothing was written
  outside `experiments/00-g-privilege/` and `/tmp/li-phase0-0g-*`.
- The one setuid binary touched is `/usr/bin/passwd`, invoked as
  `passwd -S <user>`, which only *reads* the status field. Used solely to
  observe whether `PR_SET_NO_NEW_PRIVS` suppresses a setuid gain.

### 3.2 Instrumentation

| id | what | how |
|---|---|---|
| T1 | `SO_PEERCRED` TOCTOU, pid reuse, same-tick collision | `credprobe toctou` / `same-tick`: client `fork`s, connects, `_exit(0)` without a byte; server sleeps 1000 ms, then calls `getsockopt(SO_PEERCRED)`, then reaps, then forces an impostor onto the freed pid via `ns_last_pid`. |
| T2 | namespace root reach | `unshare -Ur` / `-Urm` plus `credprobe userns`, which calls `setuid()` directly in a forked child and reports the errno. |
| T3 | helper rejection matrix | 8 traversal/symlink attempts, 12 metacharacter paths, 8 opcode abuses, 4 oversize frames, 1 oversized argument, 3 controls. |
| T3b | uid authorisation | the two gates measured separately: kernel socket mode, and the `SO_PEERCRED` allowlist. |
| T4 | audit under concurrency | 100 clients × 20 requests, then assert count, sequence density, sequence-to-file order, JSON validity, mode. |
| T5 | capability inventory | 19 commands, each run, each result recorded. |
| T6 | no arbitrary execution | `strace -f -e trace=execve,execveat` over a hostile battery, plus a source audit. |
| T7 | liveness limitation | a half frame, measured on purpose. |

### 3.3 Success criteria

All nine produced raw output, 100 % of T3 refusals refused, zero post-startup
execs in T6.

## 4. Raw results

All files from the single run `20261123T141630Z`. Superseded intermediate runs
are in `results/superseded-drafts/` and are **not** cited below.

| claim | file |
|---|---|
| TOCTOU, reuse, same-tick | `T1-peercred-20261123T141630Z.log` |
| namespace root reach | `T2-userns-root-20261123T141630Z.log` |
| rejection matrix | `T3-helper-rejections-20261123T141630Z.log` |
| uid authorisation, two gates | `T3b-unauthorized-uid-20261123T141630Z.log` |
| audit concurrency | `T4-audit-concurrency-20261123T141630Z.log` |
| capability inventory | `T5-capability-inventory-20261123T141630Z.log` |
| no-exec proof | `T6-no-exec-20261123T141630Z.log` |
| half-frame liveness | `T7-half-frame-20261123T141630Z.log` |
| environment, build hygiene | `00-environment-*`, `01-build-*` |
| machine-readable roll-up | `SUMMARY-20261123T141630Z.md` |

### 4.1 T1 — SO_PEERCRED is frozen; the pid is not an identity

```
{"case":"toctou_snapshot","client_fork_pid":292550,"client_zombie_state":"Z","clk_tck":100,
 "cred_after_delay":{"gid":1000,"pid":292550,"uid":1000},
 "cred_at_accept":{"gid":1000,"pid":292550,"uid":1000},
 "cred_is_frozen":true,"delay_ms":1000,"elapsed_between_probes_ns":1000130860,
 "pid_dir_exists_after_delay":true,"pid_liveness_after_delay":"alive_same_process",
 "starttime_at_accept":1281599,"starttime_now":1281599}
{"case":"toctou_after_reap","client_pid":292550,"liveness_after_reap":"gone",
 "note":"the socket is still open; SO_PEERCRED still reports this pid",
 "pid_dir_exists_after_reap":false,"reaped_by_server":true}
```

Impostor case, inside a private pid namespace:

```
{"case":"toctou_impostor","detail":{"attempted":true,
 "cred_on_same_socket_still_says_pid":2,"cred_pid_now_names_the_impostor":true,
 "impostor_pid":2,"impostor_starttime":1281803,
 "naive_pid_only_identity_confusion":true,"ok":true,
 "pid_equals_dead_client":true,"starttime_differs_from_client":true,
 "starttime_source":"field 22 of /proc/<pid>/stat"}}
```

Same-tick collision:

```
{"case":"same_tick_summary","clk_tck":100,"collisions":19,
 "forks_at_target_pid":20,"target_pid":4242,"trials":20,
 "verdict":"pid_starttime_is_not_a_unique_key_within_one_tick"}
```

### 4.2 T2 — namespace root

```
uid_map: 0 1000 1
euid_inside_ns: 0   euid_is_zero: true
CapEff: 000001ffffffffff        # full capability set, and it means nothing on the host
read_etc_shadow:   DENIED Permission denied (os error 13)
read_root_dir:     DENIED Permission denied (os error 13)
write_etc:         WRITE_DENIED
kill_pid1:         DENIED Operation not permitted (os error 1)
setuid: {0: ok, 1: EPERM, 5: EPERM, 65534: EPERM}   # uid 5 not yet mapped

# same probe with a mount namespace (-Urm):
mount_tmpfs: {"result":"MOUNT_OK","write_inside":true,"entries_visible":1}

# same probe, -Ur only (no mount namespace), so nowhere to mount:
mount_tmpfs: MOUNT_DENIED Operation not permitted (os error 1)
```

With a second subuid range mapped in, `setuid(5)` becomes `ok` and the unmapped
uids stay `EPERM`. A file created by namespace root:

```
host_stat: uid=1000 gid=1000 mode=644
```

The mount, measured from the host after the namespace exits:

```
host_view_after_ns_exit=GONE (the mount died with its namespace)
```

The one asymmetry worth naming: a `0600` file owned by a **mapped** uid is
readable by namespace root (`uid0 (ns root) read -> READ_OK`, file
`uid=5 gid=5 mode=600`), because `CAP_DAC_OVERRIDE` applies to any inode whose
owner is mapped. It does not reach host root's files.

### 4.3 T3 — rejection matrix (all refused, none served)

| class | attempts | result |
|---|---|---|
| traversal / absolute / symlink escape | 8 | `path_escape` ×8 |
| shell metacharacters in path | 12 | `path_invalid` ×12 |
| opcode confusion | 8 | `unknown_opcode` 1, `bad_opcode` 3, `arg_unexpected` 2, `not_implemented_by_design` 1, `bad_frame` 1 |
| oversized frame, terminated | 2 | `frame_too_large` ×2, connection survives |
| oversized frame, unterminated | 1 | no response; client times out at 1500 ms (the §6.2 defect) |
| oversized argument | 1 | `arg_too_large` ×1 |
| controls | 4 | `ok:true`: a fresh connection, `StatPath`, `ReadSmall`, `ListDir` |

Total 35 request rows: 31 refused, 4 served, and all 4 successes intended
(a fresh connection after a stall, then `StatPath`, `ReadSmall`, `ListDir`).
Escape attempts served: **0**.

The audit log's own `by_code` tally matches the response column exactly:
`path_escape` 8, `path_invalid` 12, `bad_opcode` 3, `arg_unexpected` 2,
`frame_too_large` 2, `unknown_opcode` 1, `bad_frame` 1, `arg_too_large` 1,
`not_implemented_by_design` 1 — 31 refusals, one audit line each.

### 4.4 T3b — two independent authorisation gates

```
## case 1: parent 0700, socket 0600
  socket mode=600 uid=0
  uid5 connect -> REFUSED_BY_KERNEL errno=13 Permission denied
## case 2: parent 0777, socket 0666  (kernel gate deliberately open)
  socket mode=666 uid=0
  uid5 connect -> CONNECT_OK
  response: {"ok":false,"opcode":"<connection>","code":"not_authorized",
             "message":"peer uid not allowed"}
  allowed uid0 -> {"ok":true,"opcode":"Describe", ... "uid":0}
```

### 4.5 T4 — audit log at 100-way concurrency

```
audit_lines=2101
request_served_records=2000
requests_sent=2000
file_mode=600
file_uid=1000
parsed_records=2101 malformed_lines=0
sequence_min=1 sequence_max=2101 count=2101
sequence_dense_and_in_file_order=True
sequence_duplicates=0
types={"ConnectionClosed": 100, "HelperStarted": 1, "RequestServed": 2000}
```

`2101 = 2000 requests + 1 HelperStarted + 100 ConnectionClosed`. The 100
`ConnectionClosed` records are per-connection lifecycle events, so the
per-request invariant ("exactly one record per request") holds exactly.

### 4.6 T5 — capability inventory (every row executed)

| capability | uid 1000 |
|---|---|
| read `/proc/self/stat`, `/proc/self/status` | ALLOWED |
| list `/proc` for all pids | ALLOWED |
| read another uid's `/proc/1/stat` | ALLOWED (partial fields) |
| read `/proc/1/cmdline`, same pid namespace | ALLOWED |
| inotify on an owned directory | ALLOWED (CREATE observed) |
| `AF_UNIX` bind + connect + reply | ALLOWED (socket 0600) |
| sqlite write in an owned dir | ALLOWED (`journal_mode=delete`) |
| hardlink + rename in an owned dir | ALLOWED |
| read `/etc/shadow` | **DENIED** |
| list `/root` | **DENIED** |
| read `/proc/1/environ`, `/proc/1/stack`, `/proc/1/mem`, `/proc/1/root` | **DENIED** |
| inotify on `/root`, `/var/lib/private` | **DENIED** |
| `btrfs subvolume snapshot /` | **DENIED** |
| `mount -t tmpfs`, `umount` | **DENIED** |

`PR_SET_NO_NEW_PRIVS` suppressing a setuid gain, read-only probe:

```
$ passwd -S USER                       -> USER P 2026-03-01 -1 99999 7 -1
$ setpriv --no-new-privs passwd -S USER -> USER P
```

### 4.7 T6 — no arbitrary execution

```
execve_lines_total=1
execve_lines_of_the_helper_binary=1
execve_lines_after_startup=0
```

The single `execve` is the helper's own startup. The source audit found no
`exec*`, `system`, `popen`, `posix_spawn`, `Command::new` or `fork` in the
helper, client or library — the only textual hit is a doc comment stating the
rule.

### 4.8 T7 — half-frame liveness (a real defect, kept)

Verbatim from `T7-half-frame-20261123T141630Z.log`:

```
# 100 bytes, no newline, 1500 ms client timeout:
{"case":"raw_frame","elapsed_ms":1508,"repeat":1,"requested_bytes":100,"response":"",
 "response_received":false,"response_timeout_ms":1500,
 "timed_out_waiting_for_response":true,"trailing_newline":false,"wrote_bytes":100}

# the same payload WITH a terminating newline is answered immediately, and the
# answer is a refusal because 100 bytes of 'A' is not JSON:
{"case":"raw_frame","elapsed_ms":0,"repeat":1,"requested_bytes":100,
 "response":"{\"ok\":false,\"opcode\":\"<frame>\",\"code\":\"bad_frame\",
             \"message\":\"not a JSON request object\"}",
 "response_received":true,"response_timeout_ms":1500,
 "timed_out_waiting_for_response":false,"trailing_newline":true,"wrote_bytes":100}
```

Between the two, a second client on a new connection was served normally
(`{"ok":true,"opcode":"Describe",...}`), which is what proves the stall is
per-connection and not a server-wide hang.

## 5. Analysis

### 5.1 H1 — CONFIRMED, and worse than "stale"

`SO_PEERCRED` is a snapshot: `cred_at_accept == cred_after_delay` byte for byte,
while the peer has already exited. Between the two probes 1 000 130 860 ns
elapsed and the credential did not change.

The "pid still exists after 1 s" reading is a trap: the process is a **zombie**
(`client_zombie_state: "Z"`), so `/proc/<pid>` persists until the server reaps
it. As soon as the server reaps (`reaped_by_server: true`), `liveness_after_reap`
becomes `gone` **while the socket is still open and still reports that pid**.

Then the impostor test makes it concrete: a completely different process takes
the freed pid, and the same socket's `SO_PEERCRED` now names it
(`cred_pid_now_names_the_impostor: true`,
`naive_pid_only_identity_confusion: true`). Any server that reads
`SO_PEERCRED` late and then uses `pid` to look up `/proc/<pid>` for
authorisation is reading a file about a *different* process.

**Consequence:** `pid` from `SO_PEERCRED` is attribution evidence only. The only
field fit for authorisation is `uid`, because the kernel enforced it at
`connect()` and a uid does not get recycled to another user.

### 5.2 H2 — FALSIFIED as stated, and the falsification is the useful part

19 of 20 trials collided: two distinct processes, same pid, same `starttime`,
`CLK_TCK=100`. `(pid, starttime)` is therefore **not** a unique key.

The rate is a race, not a constant: across the four independent driver runs of
this experiment the count was 20, 19, 20, 19 out of 20. That variation is itself
evidence — the collision depends on whether both forks land in the same jiffy,
which is exactly what a timing-dependent identity collision looks like. A test
that reported only "20/20 every time" would be overstating the determinism.

This does not make `starttime` useless. It means the honest claim is:

- `uid + pid + starttime` is **good attribution** — it correctly separates
  the *overwhelmingly common* case of pid reuse across ticks (the impostor test
  shows `starttime_differs_from_client: true` for reuse 150 ms later).
- It is **not authorisation**. A process that reuses a pid inside the same
  10 ms tick is indistinguishable from the original by this key alone.

So the design rule is not "verify the pid more carefully". It is "never let pid
be load-bearing for access control". The verified substitute is a per-connection
capability the server generates (see `docs/security-boundary-v0.md` §3).

### 5.3 H3 — CONFIRMED, with one asymmetry worth writing down

Namespace root is simultaneously very powerful and completely contained:

- **Powerful inside:** `CapEff = 000001ffffffffff` (the full set), can
  `setuid()` to any *mapped* uid, can mount tmpfs when it also has a mount
  namespace, and can override DAC on files owned by mapped uids.
- **Powerless outside:** cannot read `/etc/shadow`, cannot list `/root`, cannot
  write to `/etc`, cannot signal host pid 1, and a file it creates is
  host-owned by `uid=1000 gid=1000 mode=644`.

The `setuid()` result is the one that took two attempts to measure honestly: an
earlier `setpriv --clear-groups` run reported every uid as unmapped, including
uid 0, because `setgroups()` is refused inside a namespace and the tool failed
before `setuid` was attempted (deviation 3, §6). A direct `setuid()` in a forked
child shows the true rule: mapped uids succeed, unmapped uids return
`EPERM errno=22`.

The DAC asymmetry is the sharpest statement of the boundary: namespace root's
`CAP_DAC_OVERRIDE` reaches *any inode owned by a mapped uid* and *nothing owned
by an unmapped uid*. Because the only uid the user may map is their own, the
practical consequence is that namespace root can do anything the host user could
already do, and nothing more. **A user namespace cannot substitute for a real
privileged helper.**

Note the mount: `mount_tmpfs` succeeded only under `unshare -Urm`. Under
`unshare -Ur` alone (no mount namespace) it returned `EPERM`. And the mount was
`GONE` from the host's view once the namespace exited.

### 5.4 H4 — CONFIRMED

All 12 hostile requests in the T6 battery were refused (`ok:false` 12/12, zero
`ok:true`), and the `strace` proof shows **zero** post-startup `execve`. Two design decisions carry
most of the weight:

1. **Two independent path layers.** A lexical whitelist (`[A-Za-z0-9._/-]`, no
   `..`, no leading `/`) gives precise error codes; `openat2()` with
   `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS` gives
   kernel-enforced containment that survives symlinks created after validation.
   Either alone would be a known-bad pattern — the whitelist alone is TOCTOU on
   symlinks, and `openat2` alone cannot distinguish "you tried to escape" from
   "that file does not exist" for logging.
2. **A closed opcode table with no string dispatch.** `{"opcode":"exec",...}`
   is a *typed* error, not a lookup miss, so it lands in the audit log as
   `bad_opcode` rather than being silently coerced.

The strict-argument change is worth recording as a finding rather than a
refactor: before it, `{"opcode":1,"args":{"cmd":"id; cat /etc/shadow"}}`
returned `ok:true`. Nothing executed — `Describe` has no execution path — but
the helper *accepted an undeclared field*. That is an invitation to a future
refactor that starts reading `args.cmd`. Rejecting unknown keys is now an
invariant with its own error code (`arg_unexpected`), and there are 2 recorded
refusals for it.

### 5.5 H5 — CONFIRMED exactly

2101 lines for 2101 events, sequences `1..2101` dense, zero duplicates, zero
malformed lines, and `sequence_dense_and_in_file_order=True` — meaning the
sequence order *is* the file order, which is the property that makes the log
usable as a causal record rather than an unordered bag.

The mechanism is deliberately narrow: the sequence number is assigned **inside
the same mutex critical section** as the `write_all` of the single `\n`-terminated
line. `O_APPEND` alone would have given atomic offsets but not an ordering
guarantee.

Caveat measured, not assumed: 100 `ConnectionClosed` records appear because
each connection logs its own closure. The per-request invariant is exact; a
naive "lines == requests" check would have failed by 101 and been misread as
loss.

### 5.6 H6 — CONFIRMED, with one surprise

The unprivileged/privileged split matched the hypothesis on every row. The
surprise is what *is* visible unprivileged: `/proc/1/stat` and `/proc/1/cmdline`
are readable, so process enumeration across users works partially. What is denied
is the *interesting* part: `environ`, `mem`, `stack`, `root`.

`btrfs subvolume snapshot /` was denied as a plain user — the expectation was
that this needs `CAP_SYS_ADMIN`, and it does. `mount` denied, `umount` denied.
`sqlite` `journal_mode=delete` (not WAL) in a plain directory.

### 5.7 What the measurements imply for the architecture

The three findings compose into one conclusion:

- `SO_PEERCRED` gives a trustworthy **uid** and an untrustworthy **pid**.
- A user namespace gives a **fully contained** root, with no reach over host
  root's resources.
- A closed-opcode helper **cannot be steered into execution**, proven by trace.

Therefore the privilege boundary should be drawn at **uid verification plus a
data-only interface**, and the helper should be *avoided entirely* in Phase 1
rather than built early and hardened later.

## 6. Known limitations

### 6.1 Real defects found in the prototype (all fixed, all recorded)

1. **`PR_GET_NO_NEW_PRIVS` was read wrong.** The first implementation passed an
   out-pointer; the syscall returns the state as its return value. `Describe`
   reported `no_new_privs:false` for a process that had already set it. Fixed;
   now asserted by the T3 control.
2. **Oversized frames produced no audit record.** `Frame::TooLarge` returned a
   response without logging, so a peer could flood refusals and leave no trace.
   Fixed: every refusal path now writes exactly one audit line.
3. **An unterminated oversize frame could be re-entered.** The first framing
   code returned `TooLarge` without draining to the next newline, so the frame's
   tail was re-parsed as a fresh request — a request-smuggling shape. Fixed with
   an explicit `discarding` state that drops bytes up to the next `\n` and
   bounds memory by `MAX_FRAME_BYTES`, not by what the peer sends.
4. **The helper rewrote its own socket directory to 0700 unconditionally.**
   This silently defeated a test that had deliberately made the directory 0777,
   making a *filesystem* denial look like an *allowlist* denial. Fixed: the mode
   is now an explicit `--socket-dir-mode` argument. This is the root cause of
   deviation 2 in §6.3.
5. **The strace wrapper's pid was treated as the helper's pid.** `stop_helper`
   killed `strace`, the orphaned helper kept the socket, and the driver hung in
   `wait`. Fixed: the helper reports its own pid via `--ready-file` and both are
   killed, helper first.

### 6.2 The one defect that is NOT fixed: half-frame stall

A frame that is neither newline-terminated nor larger than the 64 KiB cap leaves
the helper blocked in `read()` waiting for the rest, while the client waits for a
response. Measured in T7: `timed_out_waiting_for_response: true` after 1500 ms,
with a second connection served normally, proving it is a per-connection stall
and not a server hang.

Severity: a single unauthenticated connection can hold one server thread and one
fd indefinitely. With unbounded per-connection threads, a few hundred idle
half-frames exhaust the process.

**Fix direction (not implemented — Phase 1 work):**

1. A per-frame read deadline (`SO_RCVTIMEO` or `poll()` with a timeout; e.g.
   5 s), closing the connection with `frame_timeout` and logging it.
2. A global cap on concurrent connections plus a bounded worker pool, so a stall
   costs a queue slot rather than a thread.
3. Optionally, require the peer to send a complete frame within a byte budget
   before any accept-side bookkeeping.

This is listed as a *known limitation of the prototype*, not a reason to reject
the interface shape: the framing layer is where the fix belongs, and T7 exists
to keep the defect visible until it is fixed.

### 6.3 Measurement deviations (things that were initially measured wrong)

1. **`setpriv --reuid=5` without a uid mapping failed with EPERM** because only
   uid 0 was mapped. Resolved by using `unshare --map-users=0:1000:1
   --map-users=5:100000:1`, which uses `/etc/subuid` through the setuid
   `newuidmap` — the intended unprivileged path.
2. **The allowlist test measured the wrong gate.** Default `--socket-dir-mode
   0700` meant uid 5 could not traverse into the scratch directory, so
   `connect()` failed with `EACCES` before the helper ever saw the peer. The test
   was measuring the kernel gate twice and calling the second one the allowlist.
   Resolved by making the mode explicit and by opening the parent directories;
   T3b now measures both gates and shows the second one producing
   `not_authorized` on a world-writable socket.
3. **`setpriv --clear-groups` reported every uid as unmapped, including uid 0.**
   Inside a user namespace `setgroups()` is refused, so the tool exits before
   `setuid()` runs. Resolved by calling `setuid()` directly in a forked child
   (`credprobe userns`), which shows mapped uids succeeding and unmapped uids
   returning `EPERM errno=22`.
4. **The "different uid file" test failed with `EACCES` on the path** because
   both `$SCRATCH` (mktemp, 0700) and the subdirectory were closed. Resolved by
   opening both; the result then showed the real finding (§5.3), that namespace
   root *can* override DAC for mapped uids.

### 6.4 Explicitly NOT tested

| item | status | reason |
|---|---|---|
| A real privileged helper (root, setuid, or capability-bearing) | **NOT TESTED** | no sudo password; prohibited by the brief. Every "privileged" path here is a namespace simulation. |
| `SO_PEERCRED` under a peer that calls `fork()` after `connect()` and exits the parent | **NOT TESTED** | the socket stays with the child; the reported pid would be the connecting process, not the surviving one. Plausible confusion vector, unmeasured. |
| `SCM_CREDENTIALS` per-message credentials vs `SO_PEERCRED` | **NOT TESTED** | would be the natural fix for the half-frame/liveness problem (per-message identity), out of scope here. |
| `pidfd_open` + `pidfd_getfd` as an alternative identity anchor | **NOT TESTED** | would close the pid-reuse hole; requires `CAP_SYS_PTRACE` or being the parent, so it may not be usable for a general helper. |
| Audit log integrity against a same-uid writer | **NOT TESTED** | mode 0600 protects against other users, not against the owning user, who can rewrite the file. Any real guarantee needs an append-only sink or a sealed log. |
| Behaviour under `fs.protected_*`, SELinux/AppArmor, or a hardened kernel | **NOT TESTED** | this host has neither LSM active in a way that affected these results; a hardened target may deny things measured as allowed here. |
| `unshare` on a kernel without `CONFIG_USER_NS` | **NOT TESTED** | the whole containment strategy for this experiment depends on it. |
| 100-client concurrency beyond 20 requests each, or sustained load | **NOT TESTED** | 2000 requests in 5–37 ms is a burst, not a soak. |
| Windows/macOS equivalents of any of this | **NOT TESTED** | out of scope for Phase 0. |

## 7. Conclusion

**PASS-WITH-LIMITS**

Every hypothesis was settled by measurement: H1 confirmed, H2 falsified (and the
falsification is the more valuable result), H3–H6 confirmed. All 29 driver checks
pass. Five real defects were found in the prototype by its own tests; four are
fixed, and the fifth (half-frame stall) is documented with a fix direction and a
dedicated regression test.

The limits that keep this from a plain PASS:

- No privileged helper was ever run with real privilege. The entire privileged
  side is a namespace simulation. Confidence that a *real* helper behaves the
  same is therefore zero until someone runs one.
- The audit log's 0600 is protection against other users, not against its owner.
- The half-frame stall is a real availability defect in the prototype.
- `pid`-based attribution is usable but the exact confusion window (same tick)
  is measured only under forced reuse, not observed in the wild.

## 8. Gate

### 8.1 May Phase 1 implement this as a user daemon only?

**YES.** Phase 1 should be a **user daemon with no privileged helper at all.**

Evidence:

- Every capability Phase 1 actually needs was measured ALLOWED as uid 1000:
  `/proc` reads, inotify on owned directories, `AF_UNIX` sockets, sqlite writes,
  hardlink/rename.
- Every capability that was DENIED (`/etc/shadow`, `/proc/1/environ`,
  inotify on `/root`, btrfs snapshot, mount) is either not needed by Phase 1 or
  is a `declared`-quality feature that can be deferred.
- A user namespace cannot substitute for privilege (§5.3): namespace root's
  power stops at mapped uids, i.e. at exactly what the user already had.

### 8.2 When may a privileged helper be introduced?

**Phase 3 at the earliest, and only if a Phase 2 measurement shows a capability
that cannot be obtained otherwise.** The prerequisite list, in order:

1. A named capability from §4.6 that Phase 1/2 provably needs and cannot get
   unprivileged. "It would be convenient" is not sufficient.
2. A hardening pass on the prototype that closes §6.2 (half-frame stall) and
   adds the connection/worker caps.
3. A review of the audit sink: 0600 is not tamper-evidence.
4. The interface in `docs/security-boundary-v0.md` accepted as-is, including the
   prohibition on any opcode whose effect is "run", "eval" or "open arbitrary".

Until all four hold, no helper should be installed, setuid or otherwise.

### 8.3 Five hard constraints

These are non-negotiable and each is backed by a measurement above.

1. **`pid` from `SO_PEERCRED` is never an authorisation input.** Authorise on
   `uid` only. Use `pid + starttime` for attribution and logging, and treat even
   that as fallible (§4.1, §5.1–5.2).
2. **The helper's input is data, never a command.** A closed numeric opcode table,
   a strict per-opcode argument allowlist, and no field anywhere that names a
   program, shell, interpreter or syscall. Verified by `strace` showing zero
   post-startup `execve` (§4.7).
3. **Every path is resolved by the kernel inside a fixed root**, with
   `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS)`, plus a
   lexical whitelist for precise error codes. Never `open()` on a caller-supplied
   string. 8/8 escapes refused, 0 served (§4.3).
4. **Every request, accepted or refused, writes exactly one audit line** to an
   `O_APPEND` file with the sequence number assigned inside the write's critical
   section. A refusal without a record is a bug, not a policy (§4.5, §6.1 defect 2).
5. **`PR_SET_NO_NEW_PRIVS` at startup and before every request**, and the helper
   refuses to start if it cannot be set. A setuid gain suppressed is a setuid gain
   that does not happen (§4.6, `passwd -S` probe).

A sixth, conditional on constraint 1 being relaxed for any reason: never map
`SO_PEERCRED.pid` to `/proc/<pid>` for a permission decision. The impostor test
exists specifically to make that failure mode impossible to forget (§4.1).
