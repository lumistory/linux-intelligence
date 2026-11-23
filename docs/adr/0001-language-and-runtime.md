# ADR 0001 — Implementation language and async runtime

- Status: proposed (awaiting Phase 0 evidence)
- Date: 2026-11-23

## Context

The daemon must hold many concurrent local socket clients, poll procfs on a
schedule while reacting to inotify, run sub-millisecond-critical code invoked
from shell hooks, write to SQLite transactionally, and ship as one binary.

## Decision (proposed)

Rust (stable, currently 1.99) with Tokio. Blocking work (SQLite, procfs scan,
large file I/O) is confined to dedicated threads / `spawn_blocking`, never on
the async reactor.

## Alternatives considered

| Option | Pros | Cons |
|--------|------|------|
| Rust + Tokio | memory safety, no GC pauses, mature nix/inotify/rusqlite, single binary | compile time, async/sync boundary discipline required |
| Rust, threads only | simpler model, no async colouring | many sockets + timers become hand-rolled epoll |
| Go | fast to write, good concurrency | GC pauses in hot path, less precise syscall control |
| C | minimal deps | memory-safety risk reading untrusted kernel data |
| Python | fast prototyping | unsuitable for resident daemon and hook hot path |

## Consequences

- Must define an explicit sync/async boundary per module.
- Hook-facing client must be a separate, dependency-light binary: the shell pays
  process start-up cost only, not Tokio start-up.

## Open questions

- Is a `no-tokio` client binary enough, or do hooks need a persistent helper?
  (0-A latency data decides.)
