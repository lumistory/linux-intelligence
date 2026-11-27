# ADR 0005 — Privilege model

- Status: accepted (2026-11-27; supported by 0-G + Phase 1/3 implementation)
- Date: 2026-11-23

## Implementation notes (D5)

- `li-daemon` runs strictly unprivileged as the invoking uid; optional zero-exec `li-helper` implements closed numeric opcodes (`1..3`; `4` `BtrfsSnapshot` disabled by design), `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS)`, `PR_SET_NO_NEW_PRIVS`, 2 s half-frame timeout (`P3-08`), 64-client semaphore, and dense `0600` JSONL audit logging explicitly labeled `mode_0600_owner_not_tamper_proof` (`P3-09`).

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
