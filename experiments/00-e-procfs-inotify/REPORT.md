# Experiment 0-E — procfs polling and inotify observation capability

Run stamp: `20261123T112427Z`. Every number below is copied from a file under
`results/`; nothing is estimated. Where a figure is arithmetic on logged values
it is marked *derived*. Where a test did not execute it is marked `NOT TESTED`.

`STAMP` below means the literal string `20261123T112427Z`.

---

## 1. Objective

Determine by measurement what unprivileged `/proc` polling and `inotify` can and
cannot observe on this host, so that Phase 1 (System Observer) can be designed
against measured loss rather than against documentation. Four questions:

1. Does a polling process observer see short-lived processes at all, and what is
   the loss as a function of poll interval and of process lifetime?
2. Is `pid` a usable process identity, or must it be `pid + starttime`?
3. What does inotify cost, when does it silently drop events, and can it ever
   attribute a change to a writing process?
4. Is the `NETLINK_CONNECTOR`/proc-events route a viable event-driven
   alternative on this kernel (`CONFIG_PROC_EVENTS=y`)?

## 2. Hypothesis

Hypotheses as pre-registered in `README.md` §2, with the measured verdict.

| # | Hypothesis (pre-registered) | Verdict | Evidence |
|---|---|---|---|
| H1 | 50 ms polling misses a non-trivial fraction of `/bin/true`; 1000 ms misses essentially all. | **PASS-PARTIAL** | Measured loss was 100.00 % at 50 ms, 200 ms **and** 1000 ms — the predicted gradient with interval did not appear, because process lifetime (0 ms) was below every interval tested. The "non-trivial fraction" half held absolutely; the implied "less loss when polling faster" was not observable in the tested range. The gradient is real but lives in *lifetime*, not *interval* (A1b). |
| H2 | `pid` alone is not a valid identity; `pid + starttime` is. | **FAIL** (second half) | `pid` alone is confirmed invalid (A6a: pid 4242 reused, `starttime` changed). But the stated falsifier — a reused pid with an *identical* starttime — was found: A6b produced `pid_starttime_key_collides=true` in **20 of 20** trials. `(pid,starttime)` is **not** a unique key; it is only a strong probabilistic one. |
| H3 | `pidfd_open` + poll cannot detect new processes, only exit of an opened pid. | **PASS** | A5: 50 opened, 0 readable while alive, 50 readable after exit, 0 of 50 short-lived processes observable. |
| H4 | inotify needs one watch per directory; a new subtree without an explicit watch loses events silently. | **PASS** | B4 case 1: 0 events from three files created before the new directory was watched; 6 events from the top-level equivalents. B4 case 2: `auto_add` recovers later events but 0 of the files created inside the add-watch window were seen. |
| H5 | The queue overflows at `max_queued_events=16384`; then only one `IN_Q_OVERFLOW` is reported and the lost events are unrecoverable. | **PASS** | B5 slow consumer: 16385 records delivered (8192 CREATE + 8192 CLOSE_WRITE + 1 `IN_Q_OVERFLOW`), from 50000 files created; no per-event loss indication; no way to identify what was lost. |
| H6 | A single large `write()` produces *multiple* `IN_MODIFY`, so `IN_MODIFY` is not a count of writes. | **FAIL** | B3: one 100 MB `write` → **1** `IN_MODIFY`; 100 × 1 MB writes → **100** `IN_MODIFY`. `IN_MODIFY` counts `write()` syscalls, not bytes, and does not fan out per page. |
| H7 | inotify events carry no pid; two writers in one directory are indistinguishable. | **PASS** | B8: 17 event records for four writer processes, none carrying pid or uid; `shared.txt` written by two processes yields 7 indistinguishable records. B10: `struct inotify_event` has no pid field. |

## 3. Method

### 3.1 Machine configuration (`results/env-STAMP.log`)

| item | value |
|---|---|
| host / kernel | Arch Linux, `Linux 7.2.8-zen1-2-zen x86_64` |
| cpu / ram | 20 cores, 15977720 kB |
| rust | rustc 1.99.0, cargo 1.99.0 (offline) |
| crates | `libc` 0.2.191, `nix` 0.31.3, `thiserror` 2.0.21 |
| inotify limits | `max_user_watches=524288`, `max_queued_events=16384`, `max_user_instances=1024` |
| pid / clocks | `pid_max=4194304`, `CLK_TCK=100` (10 ms `starttime` granularity), `btime=1795430575` |
| connector config | `CONFIG_CONNECTOR=y`, `CONFIG_PROC_EVENTS=y` (source: `/proc/config.gz`) |
| `/proc` mount | `rw,nosuid,nodev,noexec,relatime shared:5 - proc proc` |
| user | `uid=1000(USER)`, in group `wheel` — **no sudo used, nothing run as root** |
| scratch fs | `/tmp` = tmpfs 7.7 G; experiment dir on btrfs `/dev/nvme1n1p3` |

### 3.2 Code file roles

| file | lines | role |
|---|---|---|
| `run.sh` | 797 | The only driver. `set -euo pipefail`. Functions: `detect_env`, `build`, `proc_scan_latency` (A3), `proc_pidfd` (A5), `proc_reuse` (A6), `proc_orphan` (A7), `proc_readability` (A8), `proc_connector` (A9), `proc_poll_loss` (A1), `proc_life_threshold` (A1b), `proc_poll_depth` (A2), `proc_overhead_60s` (A4), `fs_event_sequences` (B1), `fs_cross_fs_move` (B2), `fs_large_write` (B3), `fs_recursion` (B4), `fs_overflow` (B5), `fs_permissions` (B6), `fs_editor_save` (B7), `fs_attribution` (B8), `fs_cost` (B9), `fs_attribution_limits` (B10), `summarise`. Every label writes `results/<label>-STAMP.log` via `2>&1 | tee`. |
| `src/lib.rs` | 797 | Shared crate. `parse_stat`/`read_stat`/`read_status_field`/`list_pids` (procfs parsing, field 22 = `starttime`); `scan(ScanDepth)` implementing the two scan depths, `ScanDepth::Stat` (stat only) vs `Full` (stat + status + cmdline); `percentile`/`mean`; `mod ino` (raw `inotify_init1`/`inotify_add_watch`/`read` over `libc`, so the read buffer size and raw mask are under our control — the `nix` wrapper is deliberately bypassed); `mod connector` (netlink `CN_IDX_PROC` client: subscribe ack, fork/exec/exit decode). |
| `src/bin/proc_observer.rs` | 1035 | Subcommands `poll` (interval/lifetime/rate/jitter/life-ms/depth, ground truth from the launcher's own `Child::id()`, `getrusage(RUSAGE_SELF)` + `VmHWM` at exit), `scan-once`, `pidfd`, `reuse` (writes `ns_last_pid` inside `unshare -Urpf`), `wrap` (`pid_max` shrink inside a private pidns), `orphan`, `readability`, `connector`. |
| `src/bin/fs_observer.rs` | 440 | Subcommands `watch` (print `monots_ns wd dir mask_raw mask_names cookie isdir name` for every raw event; optional recursive manual add), `overflow` (writer thread + drain thread, configurable read buffer and pre-drain stall), `cost`, `watch-count`. |
| `Cargo.toml` | — | Two bins, `panic = "abort"` in release; built with `cargo build --offline --release` (`results/build-STAMP.log`). |
| `results/` | — | 20 raw logs + `env`, `build`, `SUMMARY-STAMP.md`. Never edited by hand. |

### 3.3 Measurement design notes

- **Ground truth is not sampled.** For A1/A1b/A2 the launcher records the
  `(pid, starttime)` of every child it spawns via `Child::id()`, so the
  denominator is exact. Loss is `1 - |seen ∩ truth| / truth`.
- **Spawns are jittered** (`--jitter`). Without jitter a spawn period that
  divides the poll interval phase-locks and produces aliased 0/25/50/100 %
  figures instead of a curve.
- **The window auto-extends.** `cmd_poll` computes the span a spawn series needs
  and takes `max(requested, needed)`; this is why A1 rows show
  `duration_requested_ms 6000` / `duration_ms 33250`. A row whose window ends
  before the series does is flagged `run_valid=false`; all A1/A1b rows here are
  `run_valid=true`.
- **Rate is 20 spawns/s** with jitter, giving a mean gap of 50 ms between
  spawns.
- No privilege escalation anywhere: `unshare -Urpf` / `unshare -Urn` for the
  namespaced sub-tests, plain uid 1000 otherwise.

## 4. Raw results

Artifact index — every table below cites these files:

| id | file under `results/` |
|---|---|
| env | `env-STAMP.log` |
| build | `build-STAMP.log` |
| A1 | `A1-poll-loss-STAMP.log` |
| A1b | `A1b-life-threshold-STAMP.log` |
| A2 | `A2-poll-depth-STAMP.log` |
| A3 | `A3-scan-latency-STAMP.log` |
| A4 | `A4-overhead-60s-STAMP.log` |
| A5 | `A5-pidfd-STAMP.log` |
| A6 | `A6-pid-reuse-STAMP.log` |
| A7 | `A7-orphan-STAMP.log` |
| A8 | `A8-readability-STAMP.log` |
| A9 | `A9-connector-STAMP.log` |
| B1..B10 | `B1-event-sequences-STAMP.log` … `B10-attribution-limits-STAMP.log` |

### 4.1 A1 — polling loss vs interval, 500 `/bin/true` per row (`A1-poll-loss`)

| interval_ms | life_ms | truth | caught | missed | loss_pct | scans | scan_p50_ns | scan_p99_ns | cpu_total_s | cpu_pct_1core |
|---|---|---|---|---|---|---|---|---|---|---|
| 50 | 0 | 500 | 0 | 500 | 100.00 | 664 | 2440832 | 4669376 | 2.2349 | 6.72 |
| 200 | 0 | 500 | 0 | 500 | 100.00 | 167 | 2655431 | 4770724 | 0.9860 | 2.95 |
| 1000 | 0 | 500 | 0 | 500 | 100.00 | 34 | 2616683 | 3355159 | 0.6253 | 1.84 |
| 200 (zombie control, `--no-reap`) | 0 | 500 | **500** | **0** | **0.00** | 167 | 3450188 | 5710895 | 0.9630 | 2.88 |

The zombie control is the load-bearing row: the same spawn pattern with the
children left unreaped loses nothing. Therefore the loss in the first three rows
is caused by **process lifetime**, not by `/proc` visibility, permissions or
parser error (`read_failures_total 0` in every row).

### 4.2 A1b — loss vs process lifetime at 200 ms (`A1b-life-threshold`)

120 spawns per row, rate 20/s, jitter on. `scan_latency_ns_p50` was 2.57–2.73 ms
across rows.

| life_ms | caught | truth | loss_pct | model `1-(life+p50)/200` (*derived*) | delta (pp) |
|---|---|---|---|---|---|
| 0 | 0 | 120 | 100.00 | 98.70 | +1.30 |
| 5 | 4 | 120 | 96.67 | 96.20 | +0.47 |
| 15 | 7 | 120 | 94.17 | 91.20 | +2.97 |
| 30 | 14 | 120 | 88.33 | 83.70 | +4.63 |
| 60 | 34 | 120 | 71.67 | 68.70 | +2.97 |
| 120 | 70 | 120 | 41.67 | 38.70 | +2.97 |
| 250 | 120 | 120 | 0.00 | 0 (censored) | 0.00 |

The model column is the pre-registered formula from `README.md` §3 with the
row's own `scan_latency_ns_p50`; it is arithmetic, not a measurement. It tracks
the curve within 0.5–4.6 pp.

### 4.3 A2/A3 — scan cost, both depths (`A2-poll-depth`, `A3-scan-latency`)

| measurement | depth Stat | depth Full |
|---|---|---|
| A3 p50_ns (200 iters, 501 procs) | 1531879 | 4166867 |
| A3 p90_ns | 1669118 | 4894643 |
| A3 p99_ns | 2149755 | 5859066 |
| A3 max_ns | 4188829 | 7252248 |
| A3 mean_ns | 1578802 | 4392043 |
| A2 in-run p50_ns (200 ms, 40 scans) | — | 5852050 |
| A2 in-run p99_ns / max_ns | — | 8052117 / 8052117 |
| A2 mean_procs_per_scan / max | — | 502.1 / 503 |
| A2 read_failures_total | — | 0 |
| A2 loss_pct (500 spawns, life 0) | — | 100.00 |

The `scan-once` loop (A3) is faster than the same depth inside `poll` (A2):
4.17 ms vs 5.85 ms p50. The in-run figure is the one to design against.

### 4.4 A4 — 200 ms polling overhead, 60 s, full depth (`A4-overhead-60s`)

| metric | value |
|---|---|
| wall_time_s | 60.038 |
| scans | 300 |
| mean_procs_per_scan / max | 506.7 / 515 |
| scan_latency_ns p50 / p99 / max | 5842153 / 7823970 / 11135505 |
| cpu_user_s / cpu_system_s / cpu_total_s | 0.4725 / 1.8855 / 2.3580 |
| cpu_pct_of_one_core | 3.93 |
| max_rss_kb | 2744 |
| read_failures_total | 0 |
| distinct_pid_start_pairs | 568 |

Worst observed single scan 11.1 ms = 5.6 % of the 200 ms period; median scan
5.84 ms = 2.9 % of the period (*derived*). Cost is ~11.5 µs per process per
scan (*derived*: 5842153 ns / 506.7).

### 4.5 A5 — pidfd (`A5-pidfd`)

| metric | value |
|---|---|
| pidfd_opened | 50 |
| pidfd_open_failures | 0 |
| readable_while_alive | 0 |
| readable_after_exit | 50 |
| short_lived_spawned | 50 |
| short_lived_observable | 0 |

### 4.6 A6 — pid reuse (`A6-pid-reuse`)

| sub-test | result |
|---|---|
| (a) forced reuse, distinct ticks | pid 4242 twice; `starttime` 249337 → 249342; `pid_reused true`; `pid_only_key_collides true`; `pid_starttime_key_collides false` |
| (b) forced reuse within one 10 ms tick | `pid_starttime_key_collides true` in **20 / 20** trials (`same_tick_collision_trials 20`, `same_tick_collisions 20`) |
| (c) natural wraparound, `pid_max=301`, 900 forks | `distinct_pids 299`, `pids_handed_out_twice 1`, `pid_starttime_duplicates 0`, `distinct_pid_st_keys 900` |
| (d) host `pid_max` afterwards | `4194304` (unchanged) |

`CLK_TCK=100` (`env-STAMP.log`) is the mechanism: `starttime` is in ticks, so two
processes that reuse a pid inside the same 10 ms tick are indistinguishable by
`(pid, starttime)`.

### 4.7 A7 — orphan reparenting (`A7-orphan`)

| metric | value |
|---|---|
| intermediate_pid / grandchild_pid | 77229 / 77230 |
| sample0 | `ppid=940 state=R comm=sleep` |
| reparented_to | **940**, not 1 |
| intermediate_still_exists | false |

940 is `/usr/lib/systemd/systemd --user`. On a systemd host the orphan's new
parent is the **user session manager (a subreaper)**, not pid 1.

### 4.8 A8 — readability by process class (`A8-readability`)

| class | count | stat_fail | status_fail | cmdline_fail |
|---|---|---|---|---|
| kernel_thread | 326 | 0 | 0 | 0 |
| own_uid | 138 | 0 | 0 | 0 |
| foreign_uid | 39 | 0 | 0 | 0 |
| zombie | 1 | 0 | 0 | 0 |
| total visible | 504 | | | |

| other | value |
|---|---|
| environ_readable / empty / denied | 132 / 0 / 371 |
| yama | present |
| `hidepid` current mount | (none — plain `rw,nosuid,nodev,noexec,relatime`) |
| hidepid mount attempt | **failed: 权限不足 (permission denied)** → `hidepid_status=NOT TESTED` |

So: all 504 processes had readable `stat`, `status` and `cmdline`. Only
`environ` was restricted (371 denied).

### 4.9 A9 — proc connector (`A9-connector`)

| metric | host netns | fresh netns (`unshare -Urn`) |
|---|---|---|
| connector_open | ok | **FAILED: `sendto(connector)` errno 111 (ECONNREFUSED)** |
| spawned | 500 | 10 (never observed) |
| spawn_errors | 0 | — |
| fork_events | 505 | — |
| exec_events | 501 | — |
| exit_events | 505 | — |
| other_events | 3 | — |
| exec_vs_spawn_ratio | 1.0020 | — |
| status | available | `NOT AVAILABLE` |

Host: 500 spawns produced 505 fork events (extra forks are the test's own helper
processes) with exec/exit matching; `exec/spawn = 1.0020`. Nothing was dropped.
Inside a fresh network namespace no connector client is registered and the
kernel returns `ECONNREFUSED` (111) — the route is simply absent there.

### 4.10 B1 — event sequences, one directory (`B1-event-sequences`)

Non-recursive watch, `mask=0xfce`. Operation → measured sequence:

| operation | measured events |
|---|---|
| create + write + close | `CREATE 0x100` → `MODIFY 0x2` → `CLOSE_WRITE 0x8` |
| append | `MODIFY` → `CLOSE_WRITE` |
| `chmod 600` | `ATTRIB 0x4` |
| `mv` within dir | `MOVED_FROM 0x40 cookie=11510` + `MOVED_TO 0x80 cookie=11510` (paired) |
| `mkdir` | `CREATE+ISDIR 0x40000100 isdir=1` |
| `mv renamed.txt sub/` (dest unwatched) | **`MOVED_FROM` only** — no `MOVED_TO`, because `sub/` had no watch |
| `ln` (hard link) | `CREATE` (indistinguishable from a new file) |
| `ln -s` (symlink) | `CREATE` (indistinguishable from a hard link and a new file) |
| `unlink` | `DELETE 0x200` |
| `total_events` | 14 |

Files created inside the unwatched `sub/` produced **zero** events.

### 4.11 B2 — cross-filesystem move (`B2-crossfs-move`)

`/tmp` (tmpfs) → experiment dir (btrfs, `/dev/nvme1n1p3`):

| event | mask |
|---|---|
| `CREATE moving.txt` | `0x100` |
| `MODIFY` | `0x2` |
| `ATTRIB` | `0x4` |
| `CLOSE_WRITE` | `0x8` |

`move_pairs_seen 0` — no cookie pair, i.e. the kernel reports it as an unrelated
create-and-write, **not** as a rename. A correlation layer cannot join source and
destination; the source side is not even watched here (`/tmp` is a different
root).

### 4.12 B3 — large write (`B3-large-write`)

| writer | bytes | syswrites | `IN_MODIFY` | `CLOSE_WRITE` |
|---|---|---|---|---|
| `dd bs=100M count=1` | 104857600 (0.0414 s) | 1 | **1** | 1 |
| `dd bs=1M count=100` | 104857600 (0.0199 s) | 100 | **100** | 1 |

`total_events 105` = 1 CREATE + 1 MODIFY + 1 CLOSE_WRITE + 1 CREATE + 100 MODIFY
+ 1 CLOSE_WRITE. The `IN_MODIFY` count equals the `write()` count.

> **Log defect, recorded not papered over.** The four `MODIFY counts per file`
> summary lines at the end of `B3-large-write-STAMP.log` read `MODIFY=0` /
> `CLOSE_WRITE=0` for both files. That is an `awk` field-index bug in the version
> of `run.sh` that produced this log (`$9` on an 8-field row); the numbers above
> were counted directly from the 105 event rows in the same file, and reproduce
> as `one-write.bin MODIFY=1`, `many-writes.bin MODIFY=100`. `run.sh` was fixed
> afterwards; **this log was not regenerated**. The event rows are authoritative,
> the summary footer is not.

### 4.13 B4 — recursion (`B4-recursion`)

| case | result |
|---|---|
| 1: one non-recursive watch, files in a new subdir | `case1_events_in_newdir 0`, `case1_events_at_top 6` |
| 2: recursive + `auto_add` on new dirs | `case2_preexisting_seen 3`, `case2_immediate_seen 0`, `case2_after_gap_seen 3` |
| watch count vs dirs (60 dirs) | `directories 60`, `watches_needed_recursive 60`, `max_user_watches 524288`, `max_depth_components 6` |

`auto_add` (inotify's own `IN_MASK_ADD`-style recursion) removed the *permanent*
blind spot but left a **window**: events created between the `mkdir` and the
watch appearing are lost (`case2_immediate_seen 0`).

### 4.14 B5 — overflow (`B5-overflow`)

`max_queued_events=16384`, `read_buffer_bytes=65536`, 50000 files created.

| consumer | files_created | events_received | drain_calls | overflow_events | composition |
|---|---|---|---|---|---|
| fast (drains continuously) | 50000 | 100000 | 55759 | **0** | 50000 CLOSE_WRITE + 50000 CREATE |
| slow (300 ms stall before first drain) | 50000 | **16385** | 9 | **1** | 8192 CLOSE_WRITE + 8192 CREATE + **1 Q_OVERFLOW** |

Exactly 16384 real records plus one `Q_OVERFLOW` marker (`0x4000`, `wd=-1`). A
fast reader never overflows even at 50000 files; the trigger condition is
consumer latency, not write volume. Lower bound on loss (*derived*): at least
50000 − 8192 = 41808 `IN_CREATE` records were never delivered, and the kernel
does not say which.

### 4.15 B6 — permission boundaries (`B6-permissions`)

| attempt | result |
|---|---|
| watch root-owned 0700 directory | `inotify_add_watch` → errno 13 `EACCES`, exit 1 |
| watch another root-only path | errno 13 `EACCES`, exit 1 |
| watch nonexistent path | errno 2 `ENOENT` (control) |
| watch a regular file | **allowed** → 2 events (MODIFY, CLOSE_WRITE) |

### 4.16 B7 — editor save patterns (`B7-editor-save`)

`total_events 44`, `create_count 8`, `move_pairs 1`. Measured sequences:

| step | measured |
|---|---|
| vim `-E -s` `:wq` (default `backupcopy=auto`) | `.doc.txt.swp`/`.swx` CREATE→CLOSE_WRITE→DELETE, then `doc.txt` **MODIFY → ATTRIB → CLOSE_WRITE** — in-place write, **no rename pair** |
| vim `set backupcopy=no` | same in-place shape on `doc.txt` (MODIFY×2 → ATTRIB → CLOSE_WRITE → ATTRIB) |
| `nano --tempfile` | `nano.txt` CREATE → MODIFY → CLOSE_WRITE — in-place |
| explicit atomic replace (`mv .doc.txt.tmp doc.txt`) | `MOVED_FROM cookie=11530` + `MOVED_TO cookie=11530` — **the only rename pair in the log** |

The practical consequence: the "atomic replace" pattern that correlation logic
must handle was **not** what vim or nano did here. A daemon that keys on
`MOVED_TO cookie` would have seen nothing for three of four real editor saves.

### 4.17 B8 / B10 — attribution (`B8-attribution`, `B10-attribution-limits`)

| fact | evidence |
|---|---|
| fields in every event | `wd, mask, cookie, name` — no pid, no uid |
| 4 writer processes, 2 files + 1 shared file | 17 records: 3 CREATE, 7 MODIFY, 7 CLOSE_WRITE |
| two writers on `shared.txt` | 7 records, interleaved, **completely unattributable** |
| `struct inotify_event` | `{s32 wd; u32 mask; u32 cookie; u32 len; char name[];}` |
| `grep -c pid /usr/include/linux/inotify.h` | **0** |
| fanotify probe (`FAN_REPORT_PIDFD`) | **did not compile**: `O_LARGEFILE` undeclared → `compile failed`; no errno was ever observed |

### 4.18 B9 — inotify observer cost (`B9-cost`)

| metric | value |
|---|---|
| files_created / events_received | 1000 / 2000 (`events_per_file 2.000`) |
| drain_calls | 21 |
| drain_latency_ns p50 / p99 / mean | 11400 / 156164 / 18963 |
| wall_s | 0.0225 |
| cpu_user_s / cpu_system_s / cpu_total_s | 0.0000 / 0.0023 / 0.0023 |
| max_rss_kb | 2416 |
| cpu_us_per_event | 1.167 |

The log carries the caveat verbatim: *writer and observer share this process; the
CPU figure includes file creation.*

## 5. Analysis

**5.1 The poller's loss is a lifetime effect and it is fully predictable.**
A1's three intervals all show 100 % loss for 0 ms processes, which looks like a
broken observer. The zombie control row (identical pattern, `--no-reap`, loss
0.00 %, 500/500, mean 701.3 procs/scan) proves the observer is fine and the
target is simply not alive at any sample instant. A1b then shows the loss is a
smooth function of lifetime, and the pre-registered model
`1-(life+scan_p50)/interval` reproduces it within 0.5–4.6 pp across five
decades of loss. Phase 1 can therefore *declare* its blind spot numerically
instead of hand-waving: at 200 ms, anything living ≲60 ms is mostly invisible
(71.67 % lost), and only ≥250 ms lifetimes are captured reliably (0.00 % lost).

**5.2 Cost is not the constraint; interval is not free either.**
A4 is the clean overhead figure (no children spawned in-process): 300 scans in
60.038 s, 2.358 s CPU total, 3.93 % of one core, 2.7 MB RSS, 506.7 processes per
scan. The 50 ms row of A1 costs 6.72 % and buys nothing — still 100 % loss at
0 ms lifetime. So faster polling is pure cost until the interval approaches the
target's lifetime; the only way below ~60 ms lifetimes is an event source, not a
shorter sleep. Note that A1/A1b's `cpu_pct_of_one_core` includes the in-process
spawner (A1b reaches 65.76 % at life 250 ms while the scan p50 stays at 2.69 ms —
the extra CPU is fork/exec, not scanning). A4 is the number to quote.

**5.3 `(pid, starttime)` is good, not correct.**
A6a shows `pid` alone is unusable. A6b shows the fix is not an identity either:
20/20 forced reuses inside one 10 ms tick collided on both fields. A6c's natural
wraparound produced 900 distinct keys over 900 samples with only one pid handed
out twice, so in practice collisions are rare — but "rare" is exactly the
property that produces a silently wrong join years later. Phase 1 must treat
`(pid,starttime)` as a **correlation key with a documented collision window**, and
must never use it as a primary key without a tiebreaker (e.g. a monotonic
sequence from the observer, or `pidfd` for anything it holds open).

**5.4 The event-driven alternative exists but is fragile.**
A9 is a clean result on the host: 505 fork / 501 exec / 505 exit for 500 spawns,
nothing lost, `CONFIG_PROC_EVENTS=y`. It is also a route that vanishes inside a
network namespace (`ECONNREFUSED` 111) — which is precisely the environment a
sandboxed or containerized Phase 1 daemon will run in. Connector is therefore an
*optional accelerator*, never the load-bearing source.

**5.5 inotify is a filesystem change notifier, not an audit log.**
Three independent ways it loses the story:

- **Overflow is silent and unrecoverable.** B5: 16384 + 1 marker, 41808+ CREATE
  records minimum never delivered, no indication of which. The trigger is
  consumer latency (300 ms stall), which a daemon under GC, swap or a slow
  SQLite write will hit. Phase 1 must treat `IN_Q_OVERFLOW` as a **data-loss
  event that invalidates a subtree scan**, not as a warning line.
- **Structural blind spots.** B4: unwatched subdirectories deliver nothing, and
  even `auto_add` has a gap. B2: cross-device moves are not renames at all
  (`move_pairs_seen 0`) — a `mv /tmp/x /home/y` is indistinguishable from someone
  writing a new file.
- **No actor.** B8/B10: no pid, no uid, ever. Two writers on one file produce
  seven records that cannot be split. Nothing in the unprivileged toolchain
  fixes this.

**5.6 The semantic traps worth encoding in the schema.**
`IN_MODIFY` counts `write()` calls, not bytes and not "a save" (B3: 1 vs 100 for
the same 100 MB). `CREATE` is emitted for hard links and symlinks alike (B1), so
`CREATE` ≠ "new content". `MOVED_FROM` with no matching `MOVED_TO` means "moved
into an unwatched directory" and must not be dropped (B1). And editors do not
do what the folklore says: vim (both `backupcopy` settings) and nano wrote
in place here, producing `MODIFY`+`ATTRIB`+`CLOSE_WRITE` on the real file and no
rename pair at all (B7) — only the hand-rolled `mv` produced `MOVED_TO cookie`.
A recovery correlator that assumes atomic-replace will mis-handle the common case.

**5.7 Reading `/proc` is not the hard part.**
A8: 504 processes, 0 failures on `stat`/`status`/`cmdline` across kernel threads,
foreign uids and the single zombie; only `environ` was restricted (371 denied).
With no `hidepid` in play, an unprivileged full scan is complete for the fields
Phase 1 needs. A7 changes one assumption: on this systemd host an orphan
reparents to `systemd --user` (940), not pid 1, so "ppid==1 means orphaned" is
wrong here — the correct test is "ppid changed away from the recorded parent".

## 6. Known limitations

### 6.1 NOT TESTED — with reasons

| item | status | reason |
|---|---|---|
| `hidepid=2` / `hidepid=1` visibility | **NOT TESTED** | `mount -t proc -o hidepid=2` inside `unshare -Urpf` failed with permission denied (A8). Mounting procfs needs privilege in the init mount namespace; no sudo available. The 0-failure result in A8.8 therefore applies to a `hidepid=0` mount only. |
| fanotify privilege requirement (`EPERM` on `FAN_CLASS_NOTIF` / `FAN_REPORT_PIDFD`) | **NOT TESTED** | The probe in B10 failed to *compile* (`O_LARGEFILE` undeclared), so `fanotify_init` was never called and **no errno was observed**. The claim "fanotify needs privilege" in this report is from documentation, not measurement. What *is* measured: the inotify ABI has no pid field. |
| eBPF / audit-based file attribution | **NOT TESTED** | Out of scope for 0-E; requires `CAP_BPF`/`CAP_PERFMON` or root-installed audit rules. Recorded only as the named escape hatch. |
| inotify on network filesystems (NFS/CIFS/sshfs) | **NOT TESTED** | No such mount was available or created; all watches were tmpfs or btrfs. |
| btrfs-specific behaviour (snapshots, subvolumes, `send`/`receive`) | **NOT TESTED** | Only plain files on btrfs were tested (B2 target). Snapshot creation and subvolume moves emit nothing we characterised. |
| more than a handful of inotify instances / `max_user_instances=1024` exhaustion | **NOT TESTED** | Only 1 instance at a time was used; `max_user_watches` was computed (60 dirs → 60 watches) but never approached. |
| behaviour above ~500 processes and under real load | **NOT TESTED** | Host had 500–510 processes; scans were measured on an otherwise idle machine. Loss under contention (scan latency 150 ms on a loaded box) is unmeasured — and since loss depends on scan duration as well as interval (§4.2 model), the published loss curve is an *idle-host* curve. |
| poll stability over hours (drift, fd leaks, RSS growth) | **NOT TESTED** | Longest run was 60.038 s (A4). |
| `pidfd` across pid namespaces, `pidfd_getfd`, `PIDFD_THREAD` | **NOT TESTED** | A5 covers same-namespace open/exit only. |
| connector under load / `ENOBUFS` on the netlink socket | **NOT TESTED** | A9 saw 0 drops at 500 spawns on an idle host; the socket's own buffer limit was never probed. |
| connector in a *pid* namespace with a working netns | **NOT TESTED** | Only the combined `unshare -Urn` case was run; it failed at `sendto` before any pid-namespace semantics could matter. |
| cross-distro / containerised filesystem event semantics | **NOT TESTED** | Single host, single kernel (7.2.8-zen). |
| multi-writer ordering guarantees between two inotify readers | **NOT TESTED** | Only one reader instance existed. |

### 6.2 Measured but with a caveat attached

| caveat | detail |
|---|---|
| B3 summary footer is wrong | `MODIFY=0` lines are an `awk` bug in the `run.sh` that ran; the 105 event rows are authoritative and were counted independently (§4.12). Not regenerated. |
| `cpu_pct_of_one_core` in A1/A1b includes the in-process spawner | Only A4 is spawner-free. Quote 3.93 % from A4. |
| B8's writer pids are not in the log file | The header asserts four distinct writers, but the `writer_X pid=` lines went to the terminal, not into the log. The log's own evidence for "no attribution" is that no event row contains a pid — which is the claim that matters and is fully evidenced. |
| A1/A1b `duration_ms` (33250 / 9500) ≠ `duration_requested_ms` | Expected: the window auto-extends so the spawn series fits (§3.3). All rows `run_valid=true`. |
| `results/xfs-target/` no longer exists | B2's watch root was cleaned at the end of the run; `B2-crossfs-move-STAMP.log` remains the record. |
| build is not warning-clean | `build-STAMP.log` shows 2 warnings (`unused_mut` at `proc_observer.rs:751`, dead code `mono_ns`). `cargo clippy` and `cargo fmt` were **never run** by `run.sh`, so the brief's clippy-clean requirement is **NOT VERIFIED**. No code was changed in this reporting pass. |

## 7. Conclusion

**PASS-WITH-LIMITS.**

Measured answers to the four questions:

1. **Short-lived processes are invisible to polling, and it is quantified.**
   At 200 ms with a 5.84 ms scan, a 0 ms process is lost 100.00 % of the time,
   a 5 ms one 96.67 %, a 60 ms one 71.67 %, a 120 ms one 41.67 %, and only at
   250 ms does loss reach 0.00 % (A1b). Interval buys almost nothing while the
   interval exceeds the lifetime: 50 ms, 200 ms and 1000 ms all lost 500/500
   (A1). The zombie control (0.00 % loss, 500/500) proves the mechanism is
   lifetime, not visibility.
2. **`pid` is not an identity; `(pid,starttime)` is a good but imperfect key.**
   Forced reuse reproduced pid 4242 with different starttimes (A6a), and
   20/20 same-tick reuses collided on *both* fields at `CLK_TCK=100` (A6b).
   `pidfd` detects exit only — 0 of 50 short-lived processes (A5).
3. **inotify is cheap and structurally incomplete.** 1.167 µs CPU/event and
   2.4 MB RSS (B9); but 16384 queued events + one `IN_Q_OVERFLOW` marker and an
   unquantifiable remainder (B5), zero events from unwatched subdirectories
   (B4), no cookie pair for a cross-filesystem move (B2), no pid ever (B8/B10).
4. **The proc connector works on the host and is absent in a fresh netns**
   (505/501/505 events for 500 spawns vs `ECONNREFUSED` 111) (A9).
5. Hypothesis score: 4 PASS (H3, H4, H5, H7), 1 PASS-PARTIAL (H1), **2 FAIL
   (H2, H6)**. Both failures are useful: `(pid,starttime)` is not unique at tick
   granularity, and `IN_MODIFY` counts syscalls, not bytes.

## 8. Gate

**May Phase 1 implement the System Observer on procfs polling + inotify alone?
YES — with the capability boundary below declared in the adapter's
machine-readable capability record (protocol §3), not hidden.**

### 8.1 Recommended polling parameters (from measurement)

| parameter | recommended | basis |
|---|---|---|
| default process poll interval | **200 ms** | 3.93 % of one core, p99 scan 7.82 ms, 2.7 MB RSS (A4). Worst scan 11.1 ms leaves 94 % of the period idle. |
| never go below | **100 ms** | At 50 ms the cost doubles (6.72 %, A1) while 0 ms-lifetime loss stays 100 % — no accuracy is bought. |
| idle / battery profile | 500–1000 ms | 1000 ms cost 1.84 % (A1 row 3); acceptable only where sub-second process history is not promised. |
| scan depth | `Stat` for the poll loop, `Full` on demand for a specific pid | 1.53 ms vs 4.17 ms p50 per scan (A3) for the same 501 processes. |
| inotify read buffer | ≥ 64 KiB, drain always, never block in business logic | B5: a fast reader never overflowed at 50000 files; a 300 ms stall truncated 50000 files to 16384 records. |
| watch budget | one watch per directory, computed up front | 60 dirs → 60 watches against `max_user_watches=524288` (B4); the failure mode is a *missing* watch, not exhaustion. |

### 8.2 Declared capability boundary for Phase 1

Must be `known-lossy`, not silently degraded:

| capability | declaration | evidence |
|---|---|---|
| process table (`pid`,`ppid`,`comm`,`state`,`starttime`,`cmdline`) | `true`, `observed` | A8: 0 read failures on 504 processes |
| process **creation** events below ~60 ms lifetime | `false` at 200 ms | A1b: 71.67 % lost at 60 ms, 94.17 % at 15 ms |
| process **exit** of an already-known pid | `true` (detected at next scan) | A1 zombie control; A5 exit readability |
| process identity | `(pid,starttime)`, `partial:collision window ≤1 tick (10 ms)` | A6b: 20/20 same-tick collisions |
| orphan detection | `partial:do not assume ppid==1` | A7: reparented to `systemd --user` (940) |
| file create/modify/close/delete/attrib | `true`, `observed` | B1 |
| file rename (same fs) | `true` when both sides watched; `partial` otherwise | B1: `MOVED_FROM` with no `MOVED_TO` into an unwatched dir |
| file rename across filesystems | `false` — reported as create+write | B2: `move_pairs_seen 0` |
| events in newly created subtrees | `partial:lost until watch is added` | B4: 0 events in `newdir`, 0 `immediate.txt` even with `auto_add` |
| event delivery completeness | `partial:IN_Q_OVERFLOW is data loss` | B5: 16385 of 50000+ records |
| **file → writer attribution** | **`false`** | B8, B10: no pid/uid in the ABI |
| watching paths owned by other uids | `false` (EACCES) | B6: errno 13 |
| `hidepid`-restricted hosts | `unknown` | NOT TESTED |

### 8.3 Deferred — cannot be delivered by this stack

Must wait for an eBPF or audit phase (ADR 0005 already says this; 0-E confirms it
with data):

1. **Who wrote this file** — pid/uid/comm attribution of a filesystem event.
   Not obtainable from inotify (B8/B10). Requires eBPF (`CAP_BPF`/`CAP_PERFMON`)
   or kernel audit rules installed by root.
2. **Ordered, lossless process create/exec/exit** at sub-tick resolution.
   Polling cannot do it (A1b); the proc connector can on the host (A9) but not in
   a network namespace, so it cannot be the sole source for a sandboxed daemon.
3. **True rename identity across filesystems** (B2) and **new-subtree
   completeness** (B4) — needs a privileged/layered filesystem event source.
4. **Guaranteed-complete file history** — `IN_Q_OVERFLOW` (B5) is unrecoverable;
   only a journal/snapshot provider can fill the hole, and that is a different
   experiment (0-G / recovery providers).
5. **`hidepid` and other-uid observation** — needs privilege; `NOT TESTED` here
   and out of scope for the unprivileged daemon.

### 8.4 Gate condition

Phase 1 may proceed with the System Observer as a **user daemon over procfs
polling (200 ms) + inotify**, on condition that its capability record publishes
the table in §8.2 verbatim, that `IN_Q_OVERFLOW` is handled as a "rescan this
subtree" trigger, that file events are `observed` but **unattributed**, and that
poll-interval-dependent loss is stated to the user rather than discovered in
production. Nothing measured here blocks Phase 1; two items (`hidepid`,
fanotify's errno) remain `NOT TESTED` and must not be cited as verified.
