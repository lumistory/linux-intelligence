# 00-C — Unix Domain Socket concurrency and reliability

Status: **complete**. This README was written before any measurement (per
`docs/00-phase0-protocol.md` §0.2); the results are in `REPORT.md` and the
protocol draft derived from them is `docs/ipc-protocol-v0.md`.

## 1. Objective

Determine whether a `SOCK_STREAM` Unix domain socket, carrying one JSON object
per line, is an adequate transport for a Linux activity-recording daemon that is
attached to every interactive shell. The transport has to satisfy ten concrete
behaviours before Phase 1 is allowed to build on it. Every behaviour below is
measured, not argued:

1. **Concurrency** — 50 clients connect simultaneously, each sends 100 events.
   Server receives all of them; per-session `sequence` is verified monotonic.
   Report throughput (events/sec) and p50/p99 end-to-end latency.
2. **Peer identity** — `SO_PEERCRED` yields peer pid/uid/gid; connections whose
   uid does not match the daemon's allow-list are rejected.
3. **Message size limit** — a single 10 MB message is rejected and the daemon
   survives.
4. **Write blocking / server not reading** (`SIGSTOP` on the daemon) — how many
   writes does a client complete before it blocks, and roughly how much socket
   buffer is there. This is the evidence for "a wedged daemon must not slow down
   the shell".
5. **Daemon completely unavailable** (socket file missing / nobody listening) —
   cost of a single report attempt, 100 samples, median and p99.
6. **Daemon `kill -9` then restart** — measure client reconnect backoff, and
   show that events lost during the outage are discoverable via `sequence` gaps.
7. **Deduplication** — the same `event_id` sent twice is deduplicated and counted.
8. **Loss is detectable** — 10 000 events, daemon dies mid-stream, restart, then
   compare by `sequence` to compute how many were lost.
9. **Backpressure** — when the daemon is slow the client drops events and counts
   the drops instead of buffering without bound.
10. **Abnormal close** — client closes the fd without a shutdown handshake; does
    the daemon leak the connection? And what happens to reconnects after the
    socket file was unlinked?

## 2. Hypothesis (stated before running)

| # | Hypothesis |
|---|-----------|
| 1 | 50×100 = 5000 events are handled without loss; sustained throughput ≥ 20 000 events/sec; p99 latency < 5 ms |
| 2 | `SO_PEERCRED` returns the true pid/uid/gid; a uid-mismatched client is rejected and its events are not recorded |
| 3 | A 10 MB frame is refused with an error frame; the daemon process exits with code 0 only on request, i.e. it is still alive afterwards, and the same connection remains usable |
| 4 | A client blocks after ~150–300 small writes (≈ socket `wmem_default` 212 992 B), i.e. bounded, and the block is *inside the write syscall*, not in shell logic |
| 5 | A single report attempt against a dead daemon costs < 100 µs (ENOENT) and < 500 µs (ECONNREFUSED); p99 < 1 ms |
| 6 | Reconnect succeeds within ≤ 500 ms of the daemon coming back; the events lost during the outage appear as an exact `sequence` gap |
| 7 | Duplicate `event_id` is detected and only counted once |
| 8 | Lost-event count computed from `sequence` equals the number of events the client failed to get acked |
| 9 | With a slow daemon the client's drop counter rises and its memory stays bounded |
| 10 | No connection leak after abrupt `close()`; an unlinked socket file breaks *new* connections only, existing ones keep working |

Failures are results. Anything not executed is written `NOT TESTED` with a
reason.

### Verdicts (filled in after the run; see `REPORT.md`)

| # | Verdict |
|---|---|
| 1 | PARTIAL — no loss, 125k–137k e/s, but burst p99 34–37 ms (> 5 ms) |
| 2 | PASS |
| 3 | PASS |
| 4 | PASS — 167 small frames, blocks inside `write()` |
| 5 | PASS — p50 3.1 µs / 2.6 µs |
| 6 | PARTIAL — reconnect 1.631 s end-to-end; gap exact |
| 7 | PASS |
| 8 | PASS — 0 unexplained gaps |
| 9 | PASS |
| 10 | PASS |

## 3. Method

- Language: Rust, `rustc 1.99.0`, crates from the local cargo cache
  (`--offline`). Server: `tokio` multi-thread runtime. Clients: `std` blocking
  sockets in threads, because one client process per shell is the real topology.
- Transport: `AF_UNIX` / `SOCK_STREAM`, framing = one JSON object per line
  (`\n` terminated). Socket at `$XDG_RUNTIME_DIR/li-phase0-ipc/ipc.sock`,
  `0700` directory.
- Envelope: the shared envelope from `docs/00-phase0-protocol.md` §2
  (`schema_version`, `event_id`, `source`, `session_id`, `sequence`,
  `ts_mono_ns`, `ts_wall`, `type`, `payload`, `evidence_status`).
- Server limits: `MAX_FRAME_BYTES = 1 MiB` per line; on overflow the frame is
  discarded up to the next newline (`DISCARD_CAP = 64 MiB`), an `Error` frame
  `payload_too_large` is returned and the connection stays usable.
- Server ingest: bounded queue (default capacity 1024). The reader task awaits
  the queue, so a slow worker stops the reader, which is what propagates
  backpressure to the client. Worker acknowledges each accepted event with
  `{"type":"Ack",...}`; `Ping`/`Pong` goes through the same queue and therefore
  acts as a flush barrier.
- Every accepted event is appended to a journal (`sid`, `seq`, `eid`) with a
  flush per line, so a `kill -9` still leaves an auditable record.
- Drivers: `run.sh` (one test per invocation, `tee` to `results/`). Each
  invocation is timestamped; nothing under `results/` is edited by hand.

## 4. What is *not* measured

- No `SOCK_SEQPACKET` / `SOCK_DGRAM` comparison (out of scope for 0-C).
- No cross-kernel-version comparison (single host, kernel 7.2.8-zen).
- No multi-user concurrency beyond the single uid-mismatch test; no SELinux /
  AppArmor interaction (neither is enforcing on this host).
- Cgroup memory accounting of the daemon under sustained load is not sampled
  (only the client-side bounded-queue behaviour is measured).
- Real shell instrumentation latency (the hook cost) belongs to 00-A/00-B, not
  here; this experiment measures only the socket round trip.

## 5. Layout

```
experiments/00-c-ipc/
  README.md            # this file, written first
  run.sh               # reproducible driver, per-test subcommands
  Cargo.toml
  src/lib.rs           # envelope, framing, limits, SO_PEERCRED, percentiles
  src/bin/ipcd.rs      # daemon prototype
  src/bin/ipcclient.rs # test clients / harness
  results/             # raw logs, timestamped
  REPORT.md            # 8-section report
```
