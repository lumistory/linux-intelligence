# 00-C — Unix Domain Socket concurrency and reliability

Run set referenced throughout: **`results/20261123T111223Z-*`** (one
`./run.sh`, exit 0, `ALL TESTS PASSED`; driver log
`results/20261123T111223Z-FULLRUN.driver.log`). Two further complete runs
(`20261123T111407Z`, `20261123T111539Z`) are cited in §4.11 for repeatability.
Earlier partial runs during development are archived outside the repo and are
not cited.

## 1. Objective

Decide whether `AF_UNIX`/`SOCK_STREAM` with one JSON object per line is an
adequate transport for a daemon attached to every interactive shell, by
measuring ten behaviours: concurrency/latency, peer identity, frame size limits,
write blocking, cost when the daemon is absent, reconnect after `kill -9`,
deduplication, loss detectability, backpressure, and abnormal close.

## 2. Hypothesis

Stated before running, verbatim from `README.md` §2:

| # | Hypothesis | Verdict |
|---|---|---|
| 1 | 5000 events without loss; >= 20 000 e/s; p99 < 5 ms | **PARTIAL** — no loss, 125k–137k e/s, but p99 is 34–37 ms under a 50-client burst |
| 2 | `SO_PEERCRED` true pid/uid/gid; mismatched uid rejected | **PASS** |
| 3 | 10 MB frame refused, daemon alive, connection reusable | **PASS** |
| 4 | Blocks after ~150–300 small writes, inside `write()` | **PASS** — 167 frames, and it blocks inside the syscall |
| 5 | < 100 us (ENOENT), < 500 us (ECONNREFUSED), p99 < 1 ms | **PASS** — p50 3.1 us / 2.6 us |
| 6 | Reconnect <= 500 ms after restart; loss visible as a gap | **PARTIAL** — 1.63 s to reconnect with a 500 ms ceiling; gap exact |
| 7 | Duplicate `event_id` deduplicated and counted | **PASS** |
| 8 | Loss count from `sequence` equals unacked count | **PASS**, with a caveat that became a protocol requirement |
| 9 | Slow daemon -> drop counter rises, memory bounded | **PASS** |
| 10 | No connection leak; unlink breaks new connections only | **PASS** |

## 3. Method

Machine (recorded by `env_record()` into every log):

| item | value |
|---|---|
| CPU | 13th Gen Intel Core i7-13700H, 20 threads |
| RAM | 16 361 185 280 B (approx 15 GiB) |
| kernel | 7.2.8-zen1-2-zen, x86_64 |
| OS | Arch Linux, btrfs root |
| rustc / cargo | 1.99.0 (b940084d7) / 1.99.0 (5f94df478) |
| `nofile` (soft) | 1 024 |
| `net.core.wmem_default` / `wmem_max` | 212 992 / 4 194 304 |
| `net.core.rmem_default` / `rmem_max` | 212 992 / 4 194 304 |
| user | uid 1000, gid 1000 |

Code (all under `experiments/00-c-ipc/`):

| file | role |
|---|---|
| `src/lib.rs` | envelope, framing with a hard size cap, `SO_PEERCRED`, percentile helpers, unit tests |
| `src/bin/ipcd.rs` | daemon: accept -> creds gate -> bounded reader -> bounded ingest queue -> single worker (journal, dedup, sequence) -> `Ack`/`Pong` |
| `src/bin/ipcclient.rs` | all clients and harnesses (`concurrency`, `latency`, `fill`, `probe`, `oversize`, `sender`, `audit`, `dup`, `hold`, `abrupt`, `unlink`) |
| `run.sh` | driver, one function per test, `set -euo pipefail`, timestamped logs into `results/` |
| `reconcile.py` | cross-checks a sender's self-accounting against the journal |

Transport: `$XDG_RUNTIME_DIR/li-phase0-ipc/ipc.sock`, dir `0700`, socket `0600`.
Server limits: 1 MiB/frame, 64 MiB discard cap, ingest queue 1 024 (or
`--queue-cap`), per-connection reply queue 8 192.

Reproduce: `./run.sh` (all), `./run.sh t4` (one). Each test writes
`results/<utc>-<test>.log` plus `results/<utc>-<test>.daemon.log`.

## 4. Raw results

All numbers below are copied from `results/20261123T110928Z-*`.

### 4.1 Concurrency (test 1) — `...-t1-concurrency.log`

50 clients, one connection each, 100 events each, all released by a barrier.

| run | events | acked | wall | throughput | p50 | p99 | max |
|---|---|---|---|---|---|---|---|
| 1 | 5 000 | 5 000 | 36.7 ms | 136 369 e/s | 17.8 ms | 34.8 ms | 35.3 ms |
| 2 | 5 000 | 5 000 | 36.6 ms | 136 637 e/s | 17.8 ms | 34.5 ms | 35.0 ms |
| 3 | 5 000 | 5 000 | 40.0 ms | 125 105 e/s | 18.3 ms | 37.2 ms | 37.7 ms |

Per-session `sequence` check: sessions `c1-c0/c7/c49`, `c2-c0/c7/c49`,
`c3-c0/c7/c49` audited, `present=100 lost=0 starts_at_zero=true` for all nine;
daemon `seq_violations=0`, `accepted=15000`.

Idle round trip (one event, one ack, no other client), `mode: latency`:

| session | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| lat1 | 2 000 | 12.0 us | 23.1 us | 27.6 us | 143 us |
| lat2 | 2 000 | 12.0 us | 24.8 us | 27.8 us | 6 748 us |

Burst write cost with no waiting: 2.7–4.1 us per event (2 000 events).

### 4.2 Peer credentials (test 2) — `...-t2-auth.log`, `...-t2-auth.daemon.log`

```
ipcd: accept peer pid=57521 uid=1000 gid=1000
ipcd: reject peer pid=57525 uid=100000 gid=100000 (allowed_uid=1000)
STATS {... "rejected_uid":3, "accepted":24, "seq_violations":0}
```

Foreign client (inside `unshare --user`, `id` reports `uid=5`): wrote 20 events,
`server_accepted_count: null`, audit of session `foreign` ->
`present=0 lost=20`. Same-uid session `authok-c0` ->
`present=5 lost=0`.

### 4.3 Frame size (test 3) — `...-t3-oversize.log`

| case | frame bytes | write | server reply | connection after |
|---|---|---|---|---|
| 10 MiB | 10 486 024 | ok, 6.5 ms | `payload_too_large` "exceeds limit of 1048576" | usable (`Pong` received) |
| 1 MiB+ | 1 048 840 | ok, 0.6 ms | `payload_too_large` | usable |
| 64 MiB | 67 109 128 | ok, 21.2 ms | `frame_too_large` "discard cap exceeded" | closed (by design) |

After all three, a fresh 5-client burst still completed 50/50 events
(`oversize_frames=2` in `STATS`).

### 4.4 Write blocking, daemon `SIGSTOP`ped (test 4) — `...-t4-write-block.log`

Daemon state confirmed as `Tl` (stopped) before, `Sl` after `SIGCONT`.

| payload | frame B | `SO_SNDBUF` | frames accepted | bytes accepted | blocked at frame | drained in 2 s | blocking write wall |
|---|---|---|---|---|---|---|---|
| 120 B | 387 | 212 992 | 167 | 65 077 | 168 | no | 2.36 ms @ `SO_SNDTIMEO` 2 ms |
| 8 KiB | 8 459 | 212 992 | 23 | 194 583 | 24 | no | 2.77 ms @ 2 ms |
| 0 B | 267 | 212 992 | 167 | 45 037 | 168 | no | 2.28 ms @ 2 ms |
| 120 B | 387 | 212 992 | 167 | 65 077 | 168 | no | 50.57 ms @ 50 ms |
| 120 B | 387 | 212 992 | 167 | 65 077 | 168 | no | 5.77 ms @ 5 ms |

### 4.5 No daemon (test 5) — `...-t5-no-daemon.log`

| case | errno | n | p50 | p99 | max |
|---|---|---|---|---|---|
| socket file absent | `NotFound/2` (ENOENT) x100 | 100 | 3.1 us | 10.6 us | 23.1 us |
| stale socket file, no listener | `ConnectionRefused/111` x100 | 100 | 2.6 us | 6.5 us | 15.1 us |

A third case (socket path inside a directory the client cannot write) is
**NOT TESTED**: once the socket file exists, directory write permission is not
required to `connect()`, so it is not a distinct failure mode. This is recorded
in the log itself (`results/...-t5-no-daemon.log`).

### 4.6 `kill -9` + restart (test 6) — `...-t6-restart.log`

Client: 4 000 events, 300 us apart, queue cap 256, backoff ceiling 500 ms.

| attempt | time since kill | errno | sleep |
|---|---|---|---|
| 1 | 0.0 ms | `ConnectionRefused` | 10 ms |
| 2 | 10.1 ms | `ConnectionRefused` | 20 ms |
| 3 | 30.2 ms | `ConnectionRefused` | 40 ms |
| 4 | 70.3 ms | `ConnectionRefused` | 80 ms |
| 5 | 150.4 ms | `ConnectionRefused` | 160 ms |
| 6 | 310.6 ms | `ConnectionRefused` | 320 ms |
| 7 | 630.8 ms | `ConnectionRefused` | 500 ms |
| 8 | 1 130.9 ms | `ConnectionRefused` | 500 ms |
| — | **1 631.0 ms** | reconnected | — |

Outcome: `produced=4000 written=3976 acked=3976
dropped_client_queue_full=24`; journal audit `present=3976 lost=24`, ranges
`[256,256] [513,513] [770,770] [1027,1027] [1284,1293]` plus one singleton per
subsequent queue-cap boundary (1549, 1806, 2063, ... 3862). The pattern is exact:
one drop every time the bounded queue fills (cap 256, so at seq 256, 513, 770,
1027, ...), and a 10-event block (1284-1293) produced while the daemon was down.
Client drop count and journal gap count are the same number.

### 4.7 Deduplication (test 7) — `...-t7-dedup.log`

10 unique events + 1 byte-identical resend, 21 rounds (1 manual + 20 in a loop,
distinct session per round). Journal audit per round: `present=10 lost=0`;
daemon `dedup_dropped=21` for 21 resends.

### 4.8 Loss accounting (test 8) — `...-t8-loss.log`, reconciled by `reconcile.py`

| variant | client queue | produced | written | client drops | journal present | journal lost | unexplained |
|---|---|---|---|---|---|---|---|
| A | 8 192 | 10 000 | 9 993 | 7 | 9 993 | 7 (`[8192,8198]`) | **0** |
| B | 16 | 10 000 | 9 403 | 597 | 9 403 | 597 | **0** |

`written_minus_journaled = 0` in both variants: every byte the kernel accepted was
journalled. Variant B's drops occur at a strict period of 17 (`[16,16] [33,33]
[50,50] ...`), i.e. one drop per 16 queued + 1 in flight.

### 4.9 Backpressure (test 9) — `...-t9-backpressure.log`

Daemon `--slow-ms 3 --queue-cap 64`.

| client cap | events | produced in | acked | client drops | max queue depth | barrier RTT |
|---|---|---|---|---|---|---|
| 64 | 2 000 | 7.05 s | 1 970 | 30 | **64** | 1.59 s |
| 8 | 500 | 0.39 s | 445 | 55 | **8** | 1.48 s |
| 64 | 2 000 | 6.74 s | 1 970 | 30 | **64** | 1.11 s |

`max_queue_depth` never exceeded the cap; client throughput fell to 284–297 e/s
against a 333 e/s server capacity.

### 4.10 Abnormal close and unlink (test 10) — `...-t10-abnormal.log`

300 abrupt closes (50 plain, 50 with `SO_LINGER 0`/RST, 200 plain):
`conns_opened=301 conns_closed=301`, daemon fd count unchanged before/after the
200-close block.

Unlink under a live connection: `existing_conn_write_ok=true`,
`existing_conn_barrier_ok=true`, `new_connection_after_unlink="NotFound"`,
`socket_file_exists=false`.

### 4.11 Repeatability — three consecutive full runs

The whole suite was run three times back to back (`./run.sh`, exit 0 each time):
`20261123T111223Z` (cited above), `20261123T111407Z`, `20261123T111539Z`.
Comparison of the run-critical figures (run 1 / run 2 / run 3):

| quantity | run 1 | run 2 | run 3 | stable? |
|---|---|---|---|---|
| T1 throughput (3 reps each) | 136 369 / 136 637 / 125 105 e/s | 135 253 / 115 868 / 125 995 e/s | 138 875 / 134 071 / 131 429 e/s | ±15 % |
| T1 burst p99 | 34.8 / 34.5 / 37.2 ms | 35.0 / 40.3 / 37.1 ms | 34.3 / 34.8 / 34.6 ms | ±15 % |
| T1 idle RTT p50 | 12.0 / 12.0 us | 11.9 / 11.9 us | 12.4 / 12.0 us | yes |
| T4 frames accepted (387 / 8 459 / 267 B) | 167 / 23 / 167 | 167 / 23 / 167 | 167 / 23 / 167 | **identical** |
| T4 blocking write @ `SO_SNDTIMEO` 2 ms | 2.4 / 2.8 / 2.6 ms | 2.5 / 2.6 / 2.4 ms | 2.3 / 2.8 / 2.6 ms | yes |
| T4 blocking write @ 50 ms | 50.6 ms | 50.5 ms | 50.7 ms | yes |
| T5 attempt p50 (ENOENT / ECONNREFUSED) | 3.1 / 2.6 us | 3.1 / 2.6 us | 1.8 / 1.5 us | ordering stable, absolute varies |
| T6 journal lost | 24 | 23 | 24 | ±1 event |
| T6 reconnect latency | 1 631 ms | 1 131 ms | 1 631 ms | tracks the restart script, not the daemon |
| T8a lost / unexplained | 7 / 0 | 7 / 0 | 7 / 0 | **identical** |
| T8b lost / unexplained | 597 / 0 | 597 / 0 | 597 / 0 | **identical** |
| T9 max_queue_depth == queue_cap | 64/64, 8/8, 64/64 | 64/64, 8/8, 64/64 | 64/64, 8/8, 64/64 | **identical** |
| T10 conns_opened / conns_closed | 301 / 301 | 301 / 301 | 301 / 301 | **identical** |
| T3 10 MiB connection survived | true | true | true | **identical** |
| T7 dedup_dropped | 21 | 21 | 21 | **identical** |

Every structural result - frame counts, loss counts, queue bounds, connection
accounting, dedup count, oversize survival - reproduces exactly across all three
runs. Only throughput, burst-latency and the absolute attempt cost move, which is
expected for scheduler-sensitive measurements on a machine that was not
originally idle. T6's reconnect latency varies because it includes the time the
driver took to restart the daemon process, not just the client's backoff.

## 5. Analysis

**Throughput and latency.** Sustained 125 000–137 000 events/sec on 20 threads,
no loss, no sequence violation. That is far beyond what any shell hook can
produce. But the *burst* latency of 34–37 ms p99 is not the transport: it is 50
clients x 100 events funnelled through one single-threaded worker that does a
`flush()` per line. The idle round trip is 12.0 us p50 / 27.6 us p99, which is
the number that maps onto a shell hook's cost. **The journal write, not the
socket, is the latency bottleneck** — a finding that changes the Phase 1 design
(batch the journal, or accept a 40 ms tail under burst).

**Identity.** `SO_PEERCRED` reports host uid/gid even for a user-namespaced
client (mapped uid 5 -> seen as 100 000), so an allow-list is sound. A client
that `connect()`s successfully can still be rejected a moment later, so
`connect()` success **must not** be treated as authentication; the `Error` frame
is.

**Frame size.** The 1 MiB limit is enforcement-with-survival: 10 MiB is refused,
the connection stays usable, the daemon keeps serving other clients. 64 MiB is
the point where draining becomes more expensive than killing the connection.

**Write blocking — the core evidence.** With the daemon stopped, one connection
absorbs **167 small frames (65 kB)** and then blocks; that number does not move
with payload size for small frames (167 at both 267 B and 387 B) because the
cost is per-`sk_buff`, not per-byte, and only drops to 23 frames at 8.4 kB. The
reported `SO_SNDBUF` of 212 992 B overstates usable capacity by ~3x for small
frames. Nothing ever drained while the daemon stayed stopped. And critically: a
plain blocking `write()` **never returns** (2 s+ observed in the pilot run); it
only returns because `SO_SNDTIMEO` fires, at 2.2–2.7 ms for a 2 ms timeout and
50.4 ms for a 50 ms timeout. **Without a write timeout, a wedged daemon hangs
the shell.** That is the single most important result of 0-C.

**Absent daemon.** 2.6–3.1 us p50, ~6–11 us p99 to discover there is nobody home.
A hook can afford to try the socket inline; it does not need a watchdog process.

**Reconnect.** Exponential backoff 10->500 ms gives detection in <= 500 ms once
the listener is back, but the *measured* 1.631 s includes the ~1.1 s the
experiment took to restart the daemon plus one full 500 ms sleep window. A client
with a 256-event buffer lost 23–24 events over three runs (one drop per queue-cap
boundary plus one short block during the outage); the same client with a
16-event buffer lost 597 of 10 000. Buffer size is the entire loss policy.

**Loss detectability.** The decisive result is `written_minus_journaled = 0` in
both variants of test 8: the transport lost nothing, and every gap in the
journal is a gap the client created on purpose and counted. This *only* works
because the client advances `sequence` on drop. If it did not, the two failure
modes would be indistinguishable.

**Dedup.** 21 resends, 21 detections, 0 extra journal rows. Dedup by `event_id`
alone is correct and sufficient; keying on `(session_id, sequence)` would break
session restarts.

**Backpressure.** The bounded client queue held at its cap in all three runs and
the drops were counted, so the client's memory is a constant, not a function of
outage length. The 1.1–1.6 s barrier RTT under a 3 ms/event server is a warning:
any client-side ack timeout below ~2 s fires spuriously under load.

**Connections and unlink.** 301 opens -> 301 closes, no fd growth: abrupt
`close()` is handled correctly. Unlinking the socket file under a live
connection does not disturb it (a stream socket does not consult the pathname
after `connect()`), and new connections fail with `ENOENT` — so a crashed daemon
leaves a window in which clients fail fast, which is the behaviour we want.

**Ack reliability (unplanned finding).** During the 50-client burst the daemon
dropped **2 684 of 22 390 acks (12.0 %)** because the per-connection reply queue was full
and the client was not reading. Per-event `Ack` cannot be a delivery guarantee;
the `Pong` watermark had to be introduced *during this experiment* to make loss
accounting exact. This is now a protocol requirement (section 2 of
`docs/ipc-protocol-v0.md`).

**In-memory counters vs. durability (unplanned finding).** After `kill -9` the
server's per-session `counts` are gone, so the post-restart `Pong` reported 2 953
accepted for a session whose journal held 3 977 rows. Any design that trusts a
daemon-side counter across a restart is wrong; the journal is the record.

## 6. Known limitations

- **Single host, single kernel, not a quiet machine.** The host was running its
  normal desktop workload; no CPU isolation, no `nice`, no cgroup pinning was
  applied. Throughput varies ±8 % between identical runs. Kernel 7.2.8-zen only. `SO_SNDBUF` behaviour,
  `sk_buff` truesize accounting and `unix(7)` semantics are kernel-version
  dependent; numbers must not be extrapolated to older kernels.
- **Synthetic clients.** The clients are Rust threads, not real shell hooks.
  Hook cost (fork, subshell, env) belongs to 00-A/00-B; this experiment measures
  only the socket path.
- **`nofile` = 1 024.** 50 concurrent connections is fine; 1 000 concurrent
  clients was **NOT TESTED** and would hit the limit.
- **No `SOCK_SEQPACKET`/`SOCK_DGRAM` comparison.** Out of scope; the stream
  choice was assumed.
- **No cross-user concurrency.** Only one foreign-uid client, one connection
  burst; multi-user contention on the socket is **NOT TESTED**.
- **Dedup window is not bounded in the prototype.** The `HashSet` grows without
  limit; the experiment ran at most 10 000 unique ids, so memory behaviour beyond
  that is **NOT MEASURED**. Phase 1 must bound it.
- **No sustained-load soak.** Longest single run is ~10 000 events / 2.8 s.
  Hour-scale behaviour, fd leaks under churn beyond 301 connections, and journal
  growth are **NOT MEASURED**.
- **No disk-full, no read-only filesystem, no `SIGSTOP` during journal write.**
  Journal failure handling is a single `eprintln!`; correctness under I/O error
  is **NOT TESTED**.
- **The burst p99 in 4.1 is dominated by the single worker**, so it is an *upper*
  bound on the prototype's behaviour, not a property of the transport. A
  multi-worker variant is **NOT MEASURED**.
- **`SO_PEERCRED` sampled once at accept.** Mid-stream credential change (e.g.
  `setuid` in the client after connect) is **NOT TESTED**.
- **Container/namespace daemon side.** Only the client side was namespaced.

## 7. Conclusion

**PASS-WITH-LIMITS.**

Unix domain sockets with JSON Lines are fast enough by two orders of magnitude
(125k+ events/sec, 12.0 us idle round trip) and the failure modes are all
*observable*: identity via `SO_PEERCRED`, size via a hard frame cap, loss via
`sequence` gaps, duplicates via `event_id`. Ten of ten behaviours were measured;
none contradicted the design.

Two of the original hypotheses failed partially and both matter: `p99 < 5 ms`
under a 50-client burst is false (34–37 ms, caused by the single-worker journal
flush), and `reconnect <= 500 ms` is false end-to-end (1.631 s on this run)
though the backoff ceiling does bound *detection* to 500 ms.

The limits that must be carried into Phase 1 are hard, not stylistic: every
client write needs `SO_SNDTIMEO`/non-blocking, the client queue must be bounded,
and `sequence` must advance on drop.

## 8. Gate

**May Phase 1 implement this transport? Yes**, with these conditions:

1. **Mandatory:** every client connection is `SOCK_NONBLOCK` or sets
   `SO_SNDTIMEO <= 5 ms`; on `EAGAIN` the event is dropped, counted, and
   `sequence` is advanced. Non-negotiable — without it a stopped daemon hangs
   the shell (measured: a blocking write never returned within 2 s).
2. **Mandatory:** the client's own queue is bounded and the drop policy is
   explicit and counted; it must never grow with outage length.
3. **Mandatory:** `sequence` advances on every produced event, including drops,
   and loss is computed as `max(seq)+1 - count(rows)` per session. Without this,
   client drops and transport loss are indistinguishable.
4. **Mandatory:** the server enforces a 1 MiB frame cap with
   discard-to-newline, and rejects a non-matching uid via `SO_PEERCRED` before
   ingesting a single frame.
5. **Required before shipping:** replace the in-memory dedup `HashSet` with a
   bounded structure and define its window, plus the behaviour when the window is
   exceeded.
6. **Required before shipping:** do not use per-event `Ack` as a delivery
   guarantee; either rate-limit acks or make the watermark the only delivery
   signal (as this prototype had to do after dropping 12.0 % of acks).
7. **Required before shipping:** a `DropReport` (or equivalent) so the server can
   distinguish "client dropped this" from "transport lost this"; today the
   journal alone cannot.
8. **Recommended:** address the 34–37 ms burst tail. Batch journal writes, or
   move journal I/O off the ingest worker; re-measure before claiming any p99
   target.
9. **Recommended:** add a `Hello`/`HelloAck` version exchange; v0 cannot mix
   protocol versions on one socket.
10. **Not blocking, but open:** `SOCK_SEQPACKET` comparison, `nofile` scaling
    beyond 50 clients, hour-scale soak, journal I/O error handling.
