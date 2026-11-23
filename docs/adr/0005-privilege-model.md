# ADR 0005 — Privilege model

- Status: proposed
- Date: 2026-11-23

## Context

Observing one's own processes and files needs no privilege. Some desirable
capabilities do (watching other uids, snapshotting filesystems, mounting).
Running the whole daemon as root is unacceptable.

## Decision (proposed)

User daemon by default, zero privilege. A privileged helper is a separate,
optional, later-phase component with:

- a fixed operation-code whitelist — never a command string, never a path that
  can escape a declared root;
- full argument validation (canonicalise, refuse traversal, refuse symlink
  escapes, refuse oversized payloads);
- `PR_SET_NO_NEW_PRIVS`;
- one append-only audit line per request, written before the action;
- caller identity = uid + pid + `/proc/pid/stat` start time, re-checked at
  request time (SO_PEERCRED alone is a TOCTOU hazard, see 0-G).

## Consequences

- Phase 1 ships a user daemon only. Features needing privilege are absent, not
  silently degraded.
- Kernel-level attribution (who modified this file) is out of scope until an
  eBPF/audit phase; file events are `observed` but unattributed.
