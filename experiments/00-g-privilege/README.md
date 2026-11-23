# Phase 0 / Experiment 0-G — Privilege model and a simulated privileged helper

Status: **method written before execution** (per `docs/00-phase0-protocol.md` §0.2).
Every number in `REPORT.md` comes from a file in `results/`. Anything not run is
`NOT MEASURED` / `NOT TESTED`.

## 1. Objective

Decide, by measurement, whether the system being built needs a privileged
component at all, and if so what the smallest honest interface to it is.

Four Phase 1 design questions depend on this experiment:

1. Is `SO_PEERCRED` a usable authorisation primitive, or can the pid it reports
   be recycled between `connect()` and the server's `getsockopt()`?
2. What does "root" mean inside an unprivileged user namespace? Does a
   namespace-root process hold any real power over the host?
3. Can a helper that accepts *data* (an opcode plus validated arguments) be
   built with no path to arbitrary command execution?
4. Which of the intended capabilities are achievable as a plain user daemon,
   and which genuinely require privilege?

The deliverable that matters most is a **negative one**: a defensible list of
things the Phase 1 user daemon must never be asked to do, and the trust
boundary that justifies it.

## 2. Hypotheses (stated before running)

| # | Hypothesis | Falsified if |
|---|---|---|
| H1 | `SO_PEERCRED` returns a **frozen** credential snapshot taken at `connect()`; after the client exits the reported pid is stale, and that pid can be reused by an unrelated process. Therefore `pid` alone must never be an authorisation key. | The reported pid is a live lookup that updates, or the pid cannot be reused while the socket is alive. |
| H2 | `pid + starttime` disambiguates reuse in normal operation, but at `CLK_TCK=100` two processes can share both `pid` and `starttime` within one tick, so the triple `uid + pid + starttime` is *good attribution* and *weak authorisation*. | Reuse within one tick is impossible, or a reused pid always yields a different `starttime`. |
| H3 | Namespace-root (`unshare -Ur`, uid 0 mapped to host uid 1000) can mount a tmpfs inside its own mount namespace, but cannot read host-root-only files (`/etc/shadow`), cannot signal host-root processes, and every file it creates on shared storage is owned by the unprivileged host uid. Namespace root != host root. | Namespace root reads `/etc/shadow`, or a file it creates is host-owned by a non-user uid. |
| H4 | A helper whose entire input surface is `{opcode: u8, args: typed}` can be made to reject shell metacharacters, absolute paths, `..` traversal, symlink escapes, oversized frames and unknown opcodes — with **no** code path that reaches `exec`. | Any accepted request results in an `execve()` of a caller-influenced program, or a traversal/escape is accepted. |
| H5 | One JSON line per request in an `O_APPEND` file, sequenced under a lock, loses and reorders **nothing** at 100 concurrent clients: line count equals request count and sequence numbers are dense. | Line count != request count, or sequence numbers have gaps/duplicates/misordering. |
| H6 | `/proc` reading, inotify on own directories, `AF_UNIX` sockets and sqlite writes all work unprivileged; observing other users' processes in full, watching other users' directories, btrfs snapshots and `mount` all require privilege. | Any listed "unprivileged" capability fails as a plain user, or any listed "privileged" capability succeeds. |

## 3. Method

### 3.1 Containment (hard rules for this experiment)

There is no sudo password. **No real privileged operation is performed.**
Everything that needs "root" runs inside `unshare`-created user namespaces, and
everything that needs a second uid uses a second mapping from `/etc/subuid`
(`USER:100000:65536`), which is an unprivileged range owned by the same user.

- No `setuid` file is created. No `/etc` file is modified. Nothing is written
  outside `experiments/00-g-privilege/` and `/tmp/li-phase0-0g-*`.
- The single unavoidable `setuid` interaction is *reading* the behaviour of
  `/usr/bin/passwd` with and without `PR_SET_NO_NEW_PRIVS`; it is used as a
  read-only probe (`passwd -S`, which changes nothing).
- All scratch paths: `/tmp/li-phase0-0g-*` and `experiments/00-g-privilege/results/`.

### 3.2 Tests

| id | what | how |
|---|---|---|
| T1 | `SO_PEERCRED` TOCTOU | Server accepts, sleeps 1000 ms, *then* calls `getsockopt(SO_PEERCRED)`. Client connects and `_exit(0)` immediately without a byte of protocol. Server then checks `/proc/<pid>` existence, `starttime`, and whether the pid now belongs to a different process. |
| T2 | pid reuse | Inside a private pid namespace, `ns_last_pid` is written to force a specific pid, so a *different* process reuses the dead client's pid. Compare `(pid, starttime)` before and after. |
| T3 | same-tick collision | Force reuse at the same pid within one `CLK_TCK` tick; count how often `(pid,starttime)` collides. |
| T4 | namespace root vs host root | In `unshare -Ur`: read `/etc/shadow`; read a host uid-1000 file; `mount -t tmpfs`; `setuid()` to a mapped and to an unmapped uid; signal a host root process; create a file and inspect its host-side ownership. |
| T5 | helper opcode allowlist | Rust helper: numeric opcode enum, typed args, `openat2(RESOLVE_BENEATH\|RESOLVE_NO_SYMLINKS\|RESOLVE_NO_MAGICLINKS)` rooted at one directory, `PR_SET_NO_NEW_PRIVS` at startup and again per request. |
| T6 | helper rejection matrix | Non-authorised uid; `../../etc/shadow`; `/etc/shadow` absolute; symlink to a sensitive file; 2 MiB frame; huge no-newline frame; unknown opcode (numeric and string); shell metacharacters in path args; 5000-byte argument. |
| T7 | no-exec proof | `strace -f -e trace=execve,execveat` around a full battery of requests; must show zero `execve` after the helper's own startup. Plus a source-level audit. |
| T8 | audit log | 100 concurrent clients × N requests; assert line count, dense sequence, valid JSON per line, file mode 0600, no partial lines under concurrent append. |
| T9 | minimal privilege inventory | Per-capability command as plain user, with result and errno. |

### 3.3 Success criteria

The experiment passes if T1–T9 all produced raw output, and if the helper
rejects **100 %** of the rejection matrix in T6 with zero execs in T7.

## 4. Layout

```
experiments/00-g-privilege/
  README.md              this file
  run.sh                 driver, set -euo pipefail, non-zero exit on failure
  src/                   Rust crate `li-privhelper`
    src/lib.rs           opcodes, validation, path resolution, audit log
    src/proto.rs         wire types
    src/bin/helper.rs    the simulated privileged helper
    src/bin/client.rs    test client (raw frames, concurrent mode)
    src/bin/credprobe.rs T1/T2/T3 SO_PEERCRED and pid-reuse harness
  results/               raw logs, timestamped
  REPORT.md              8-section report
```

## 5. Deviation policy

If a test cannot run in this environment it is recorded as `NOT TESTED` with the
reason, never replaced by a simulation presented as a measurement.
