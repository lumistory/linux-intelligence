# Phase 0 index (living document, updated as experiments land)

| Exp | Title | Report | Status |
|-----|-------|--------|--------|
| 0-A | Bash/Zsh/Fish interactive hooks | `experiments/00-a-shell-hooks/REPORT.md` | in progress |
| 0-B | Non-interactive script lifecycle | `experiments/00-b-script-lifecycle/REPORT.md` | in progress |
| 0-C | UDS concurrency and reliability | `experiments/00-c-ipc/REPORT.md` | PASS 10/10 |
| 0-D | PTY cross-shell compatibility | `experiments/00-d-pty/REPORT.md` | in progress |
| 0-E | procfs and inotify | `experiments/00-e-procfs-inotify/REPORT.md` | in progress |
| 0-F | Distro compatibility (Arch/Ubuntu/Debian/Fedora) | `experiments/00-f-distro-compat/REPORT.md` | in progress |
| 0-G | User privilege and simulated privileged helper | `experiments/00-g-privilege/REPORT.md` | in progress |
| 0-H | Dedup, correlation, Operation Context | `experiments/00-h-correlation/REPORT.md` | in progress |

## Phase 0 deliverable checklist

- [ ] Architecture Decision Records (`docs/adr/`) — 6 written, all still `proposed`
- [ ] Shell Compatibility Matrix — from 0-A
- [ ] Script Observation Report — from 0-B
- [ ] IPC Protocol Specification — `docs/ipc-protocol-v0.md` (draft exists, 0-C PASS)
- [ ] PTY Compatibility Report — from 0-D
- [ ] System Observer Report — from 0-E
- [ ] Security Boundary Specification — `docs/security-boundary-v0.md` — from 0-G
- [ ] Event Correlation Specification — from 0-H
- [ ] Performance Benchmark — numbers embedded in each report
- [ ] Phase 1 Implementation Specification — to be written after all gates

## Findings that already constrain Phase 1

1. **A wedged daemon can hang the shell.** A blocking `write()` with no
   `SO_SNDTIMEO` never returned (>2 s) against a SIGSTOPped daemon. Every client
   connection must be non-blocking or carry `SO_SNDTIMEO <= 5 ms`. (0-C)
2. **Per-event `Ack` is not a delivery guarantee** — 12.0 % of acks were dropped
   under a 50-client burst. Delivery proof must be a watermark/count. (0-C)
3. **Short-lived processes are invisible to polling** — at a 200 ms interval,
   processes living 5 ms were missed 96.67 % of the time; only >= 250 ms
   lifetimes were reliably caught. (0-E)
4. **inotify overflow is silent and unrecoverable** — at the measured limit
   exactly 16384 events survived, one `IN_Q_OVERFLOW` arrived, the rest were
   gone with no way to enumerate them. (0-E)
5. **(pid, starttime) is not a guaranteed unique process key** — `CLK_TCK=100`
   gives 10 ms granularity, and same-tick collisions were reproduced. (0-E)
6. **inotify cannot attribute a change to a process**, and non-privileged
   fanotify returns EPERM. Attribution needs eBPF/audit, a later phase. (0-E)
7. **awk must not fork on every command** — a `$(...)` in a bash hook costs
   ~190 µs per command; the adapter must avoid command substitution. (0-A)
8. **A dead pipe sink kills the shell** — writing to a closed pipe from a bash
   hook SIGPIPEs the interactive shell to death; a real `connect()` with a
   timeout is the safe failure mode. (0-A)
