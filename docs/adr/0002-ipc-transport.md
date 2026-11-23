# ADR 0002 — IPC transport and encoding

- Status: proposed — supported by experiment 0-C (PASS 10/10)
- Date: 2026-11-23

## Context

Shell hooks and observers report events to a user daemon, must never block the
shell, and must survive daemon restart. Transport must be local-only and
authenticated with kernel credentials.

## Decision (proposed)

`AF_UNIX` `SOCK_STREAM` at `$XDG_RUNTIME_DIR/linux-intelligence/daemon.sock`,
newline-delimited JSON (one object per line), version field in every message.

Mandatory client rules derived from 0-C measurements:

1. Every client connection is `SOCK_NONBLOCK` **or** sets `SO_SNDTIMEO <= 5 ms`.
2. Bounded outbound queue with explicit drop counting; `EAGAIN` means drop +
   increment counter + advance `sequence`, so the gap is detectable.
3. Per-event `Ack` is **not** a delivery guarantee (0-C measured 12.0% ack loss
   under burst). Delivery proof is a periodic watermark / count only.
4. Daemon verifies client uid via `SO_PEERCRED` plus `pid + starttime` (ADR 0005).

## Alternatives considered

| Option | Pros | Cons |
|--------|------|------|
| UDS SOCK_STREAM + JSONL | trivial to debug, diffable, text tools work | larger frames, no native framing |
| UDS SOCK_SEQPACKET | preserves message boundaries | not benchmarked here, partial-read edge cases |
| UDS SOCK_DGRAM | no connection state, natural drop | silent drop, no backpressure signal |
| Abstract-namespace socket | no filesystem permission issues | no ACL, harder to reason about leftovers |
| TCP loopback | portable | port management, weaker peer auth |
| CBOR / bincode | compact | opaque, harder to audit, marginal gain for text events |

## Consequences

- A single blocking write without a timeout hung for >=2 s when the daemon was
  SIGSTOPped. This is the most important constraint in the project: the hot path
  must be non-blocking, bounded, and lossy-on-failure by design.
- Measured 116k-139k events/s (50 clients x 100 events), idle round-trip
  p50 12.0 us / p99 27.6 us. Headroom is large; drop policy correctness matters
  more than raw speed.
- Protocol spec: `docs/ipc-protocol-v0.md`.
