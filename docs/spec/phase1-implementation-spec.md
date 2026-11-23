# Phase 1 Implementation Specification (draft v0)

Status: **draft**, written 2026-11-23 from the Phase 0 results available at that
time. Sections marked `[pending 0-X]` will be filled once that experiment's
report is accepted. Nothing in this document may contradict a Phase 0 gate.

---

## 0. What Phase 0 already decided for us

These are not choices any more; they are constraints with measurements behind
them. Phase 1 violates them at its own risk.

| # | Constraint | Measured basis |
|---|------------|----------------|
| C1 | Every IPC client connection is non-blocking **or** sets `SO_SNDTIMEO <= 5 ms`. A bounded queue drops and counts; `sequence` advances on drop. | 0-C T4: a plain blocking write to a SIGSTOPped daemon **never returned** (>2 s). |
| C2 | Per-event `Ack` is not delivery proof. Delivery is proven by watermark/count. | 0-C T1: 2 684 of 22 390 acks dropped (12.0 %) under a 50-client burst. |
| C3 | `event_id` alone is the dedup key; dedup on `semantic_fingerprint` is an extra relation, never a replacement. | 0-C T7: 21/21 resends deduplicated on `event_id`. `[pending 0-H]` |
| C4 | File events and process events are `observed` but **unattributed**. No "who modified this file" claim may be made. | 0-E B8/B10: inotify ABI carries no pid/uid; non-privileged fanotify returns EPERM. |
| C5 | Process identity is `(pid, starttime)` **plus** a collision flag; a same-tick collision downgrades the relation to `Unknown`. | 0-E A6b: 20/20 same-tick collisions at `CLK_TCK=100`. |
| C6 | Process **creation** below ~60 ms lifetime is not observable by polling; the capability record must say so. | 0-E A1b: 71.67 % lost at 60 ms, 96.67 % at 5 ms. |
| C7 | `inotify` overflow is data loss, not a hiccup. The reader drains unconditionally and never blocks on business logic. | 0-E B5: 16 385 delivered of 50 000+, one `IN_Q_OVERFLOW`. |
| C8 | Script observation has **two tiers** that must never be merged into one event type: lifecycle (trustworthy) vs internal commands (unavailable). | 0-B §4.6–4.7: 4 000 command invocations produced **2** events. |
| C9 | The shell hook must not fork per command. | 0-A: a `$(...)` per command costs ~190 µs. `[pending 0-A report]` |
| C10 | The hook must never write to a pipe whose reader is gone. | 0-A: raw write to a dead pipe **killed the interactive shell** via SIGPIPE. |
| C11 | Phase 1 is a user daemon with **zero** privilege. No setuid, no root, no CAP_*. | 0-G: `[pending]` |
| C12 | Provider selection is by **capability probe**, never by distro name. | 0-F: `/bin/sh` is dash on Debian/Ubuntu and bash on Fedora/Arch; glibc differs 2.39–2.44. |

## 1. Crate layout

```
linux-intelligence/
  crates/
    li-proto/        # event envelope, message types, versioning. no I/O.
    li-journal/      # SQLite schema, migrations, append-only writer.
    li-hook/         # tiny, no-tokio binary used by shell adapters.
    li-daemon/       # tokio daemon: sockets, ingest, observers, scheduler.
    li-cli/          # `li` user-facing command (run, query, checkpoint, ...).
    li-recovery/     # provider trait + git/file providers (Phase 5 mostly).
    li-policy/       # L0-L4 classification (skeleton only in Phase 1).
  shell/
    bash/ zsh/ fish/ # adapters, sourceable, no build step
```

`li-hook` must build with `panic = "abort"`, no Tokio, no serde derive if a
hand-rolled encoder is cheaper — its startup time is charged to every command.

## 2. Storage

Single SQLite database at `~/.local/state/linux-intelligence/journal.db`,
WAL, `busy_timeout` set explicitly, foreign keys on.

- `events` is append-only, enforced by triggers that raise on UPDATE/DELETE.
- Derived tables (`event_relations`, `operations`) are rebuildable.
- Schema version table + forward-only migrations, each tested on a populated
  previous-version database.
- Redaction happens before insert. `[pending 0-H]` for the measured false-positive
  rate of the redactor.

## 3. Daemon

- Tokio, one accept loop per socket, bounded ingest queue, **single writer
  thread** for SQLite (measured bottleneck is the journal flush, not the socket).
- Observers are independent tasks with their own failure budget: a panicking
  observer must not take down the shell ingest path.
- On start: detect a crash by a sentinel row, emit `DaemonRestarted`, reconcile
  `sequence` gaps for every known session, journal the loss as an event.

## 4. Shell adapters

Per ADR 0004. Each adapter emits on FD 3 and must satisfy:

1. No command substitution per command.
2. No blocking write; if `li-hook` is slow or absent, drop.
3. Never SIGPIPE the shell: check the sink is alive before writing, or ignore
   SIGPIPE **only** in a way that does not leak the ignore flag into children.
4. Declare its capability record at `SessionStarted`.
5. `unknown` is the default for anything not measured in 0-A.

## 5. Explicit non-goals for Phase 1

- No privileged helper, no snapshotting of real filesystems, no destructive
  restore.
- No eBPF, no kernel audit, no file→process attribution.
- No AI, no model calls, no prompt logic. The API is read-only except for
  approval submission.
- No Nushell adapter, no Windows/macOS.

## 6. Acceptance criteria (Phase 1 done = all of these measured, not asserted)

1. Bash/Zsh/Fish each produce correct `CommandStarted`/`CommandFinished` for
   the 13-case matrix from 0-A.
2. `li run -- cmd` produces lifecycle events; the capability record says
   `internal_script_commands: false`.
3. Killing the daemon mid-session: shell stays responsive, loss is counted and
   reported as a gap after restart.
4. `kill -9` the daemon: `PRAGMA integrity_check` returns ok on next start.
5. 10 000 shell events over 10 minutes produce zero unexplained gaps.
6. A redaction test corpus produces zero secrets in the journal.
7. Every query API returns `evidence_status` and, for relations, a confidence
   level; no response may contain the word "caused".
