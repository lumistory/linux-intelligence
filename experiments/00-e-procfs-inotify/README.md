# Phase 0 / Experiment 0-E — procfs and inotify observation capability

Status: **method written before execution** (per `docs/00-phase0-protocol.md` §0.2).
Anything the run could not establish is marked `NOT TESTED` / `NOT MEASURED` in
`REPORT.md`; no number in the report is estimated.

## 1. Objective

Determine, by measurement rather than by reading documentation, what procfs
polling and inotify can and cannot observe on this host. The question is not
"does the API work" (it does) but "what fraction of reality does it lose, and
where does it lie to us".

Concretely, four things must be settled because Phase 1 (System Observer) will
be designed on top of them:

1. Can a polling process observer see short-lived processes at all, and what is
   the loss rate as a function of poll interval?
2. Is `pid` a usable process identity, or do we need `pid + starttime`?
3. What does inotify cost, what does it drop, and can it ever attribute a change
   to a process?
4. Is `NETLINK_CONNECTOR`/proc-events a viable event-driven alternative on this
   kernel, given `CONFIG_PROC_EVENTS=y`?

## 2. Hypotheses (stated before running)

| # | Hypothesis | Falsified if |
|---|---|---|
| H1 | `/proc` polling at 50 ms misses a measurable, non-trivial fraction of `/bin/true` processes; at 1000 ms it misses essentially all of them. | 50 ms loses 0%, or loss is not monotone in interval. |
| H2 | `pid` alone is not a valid process identity; `pid + starttime` (field 22 of `/proc/<pid>/stat`) is. | A counterexample shows a reused pid with an identical starttime, or `starttime` is not per-exec stable. |
| H3 | `pidfd_open` + poll cannot detect *new* processes; it can only report *exit* of an already-opened pid. | pidfd yields creation events. |
| H4 | inotify requires one watch per directory; a recursively created subtree without explicit `inotify_add_watch` per directory silently loses events. | Events from an unwatched new subdirectory are delivered. |
| H5 | inotify's queue can be overflowed at the configured `max_queued_events=16384`, after which events are lost with no per-event indication (only one `IN_Q_OVERFLOW`). | Overflow cannot be triggered by a plausible workload, or the kernel reports which events were lost. |
| H6 | A single large `write()` produces multiple `IN_MODIFY` events (not one), so `IN_MODIFY` is not a count of writes. | Exactly one `IN_MODIFY` per `write()` syscall regardless of size. |
| H7 | inotify events carry no PID; two writers in one directory are indistinguishable. | An inotify event exposes the writer's identity. |

## 3. Method

All work happens in `experiments/00-e-procfs-inotify/` and `/tmp/li-phase0-0e-*`.
No sudo, no root, no writes outside those paths. Raw output is captured with
`2>&1 | tee results/<name>-<utc>.log`; `run.sh` regenerates everything.

### A. Process observer (procfs)

`src/bin/proc_observer.rs` — a supervisor with four subcommands:

- `poll --interval-ms N --duration-ms M` — scan `/proc/<pid>/stat`,
  `/status`, `cmdline`; maintain a map keyed by `(pid, starttime)`; emit
  `ProcessSeen` / `ProcessGone` records; record per-scan latency internally and
  emit p50/p99/max, total scans, total CPU time (`getrusage`), peak RSS.
- `pidfd` — for a spawned child set, `pidfd_open` each child and `poll()` the
  fds to show that only exit (via `POLLIN` becoming readable) is observable,
  and that a process that exists for less than the open()→open() window is
  missed entirely.
- `reuse` — construct a real PID reuse. Inside `unshare -Urpf` (a user + pid
  namespace where the uid is mapped to root, hence `CAP_SETPCAP`-equivalent
  rights over that namespace) write `ns_last_pid` to force the next pid, fork
  a child, record `(pid, starttime)`, let it exit, force `ns_last_pid` to the
  same value again, fork again, record `(pid, starttime)`. If the pid repeats
  and the starttime differs, pid-only identity is falsified as a unique key.
- `orphan` — spawn a child that forks a grandchild, exit the middle process,
  and show the grandchild's `ppid` transitioning to 1 (or to the subreaper).

The short-lived-process loss test: launch 500 `/bin/true` sequentially from the
driver while `poll` observes at interval X, then compare the set of
`(pid,starttime)` pairs the observer saw against the ground truth set captured
by the launcher itself (the launcher knows every pid it spawned via `Child::id()`,
so ground truth is exact and not itself sampled). Loss rate = 1 - |seen ∩ truth| / 500.
Intervals: 50, 200, 1000 ms. 500 spawns are done twice per interval (children of
a shell loop and a direct `execve` loop) if the first pass shows all-or-nothing
behaviour that suggests the shell, not the kernel, is the bottleneck.

Important confounding factor, recorded in advance: `/bin/true` execution time on
this host is on the order of 1 ms, and a `/proc` full scan on a ~700-process
host is on the order of 10 ms. Loss is therefore expected to be dominated by
"process lifetime < scan period". The measurement must separate *scan cost* from
*loss*: a scan slower than the interval cannot sample more often than its own
duration.

Overhead run: `poll --interval-ms 200 --duration-ms 60000`, measuring
`getrusage(RUSAGE_SELF)` utime+stime and `VmHWM`, plus the in-process p50/p99 of
a single full scan.

Unreadable processes: enumerate `/proc/*` and classify read failures of
`stat`/`status`/`cmdline`/`environ` for (a) other-uid processes, (b) zombies,
(c) kernel threads. `hidepid` is expected to be `NOT TESTED` because mounting a
second procfs with `hidepid=2` requires a mount in the init mount namespace; a
`unshare -Urpf` attempt is made first and the errno recorded if it fails.

### B. Filesystem observer (inotify)

`src/bin/fs_observer.rs` — raw `inotify_init1` + `inotify_add_watch` through
`libc` (the `nix` wrapper's 4 KiB read buffer and `from_bits_truncate` are
unacceptable for an overflow test), so we control the read buffer size, can
distinguish `IN_IGNORED` from real events, and can print the raw `mask`/`cookie`.
Subcommands:

- `watch --root DIR --events N` — print every raw event as
  `ts_mono_ns wd mask cookie name`, watch directory non-recursively or
  recursively-with-manual-add, so the "new subdirectory" case can be compared.
- `overflow --root DIR --files N` — saturate the queue; a deliberately slow
  consumer (sleep between reads) is needed to actually overflow, since a fast
  reader never lets the queue fill. Both a fast and a slow consumer are run so
  the trigger condition is stated as measured.
- `cost` — measure observer CPU (`getrusage`) across 1000 creates.

Scenario coverage: create, chmod, write, unlink, `mv` same-directory, `mv`
across directories inside one watch root, `mv` from a different filesystem
(`/tmp` tmpfs → btrfs, expected to degrade to CREATE+DELETE), hard link, symlink,
100 MB single write, vim ex-mode save (temp file + rename), and two concurrent
writers in one directory to demonstrate the missing-PID limitation.

## 4. Expected artifacts

```
experiments/00-e-procfs-inotify/
  README.md            this file
  run.sh               full driver, set -euo pipefail
  Cargo.toml
  src/bin/proc_observer.rs
  src/bin/fs_observer.rs
  results/             timestamped raw logs + CSV/markdown tables
  REPORT.md            the 8 mandatory sections
```
