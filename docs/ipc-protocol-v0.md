# Linux Intelligence IPC Protocol — v0 (draft)

Status: **draft**, derived from the measurements in
`experiments/00-c-ipc/REPORT.md`. Every numeric limit below is either copied
from a measured result or marked `TBD` because it was not measured.

Transport assumptions under test: `AF_UNIX`, `SOCK_STREAM`, one JSON object per
line, socket at `$XDG_RUNTIME_DIR/li-phase0-ipc/ipc.sock`, directory mode
`0700`, socket mode `0600`, owner = the recording user.

---

## 1. Wire format

- Framing: **one JSON object per line, `\n` (0x0A) terminated.** A frame is
  never split across lines; a `\n` inside a JSON string must be escaped as
  `\u000a` (standard JSON escaping does this).
- Encoding: UTF-8, no BOM.
- A frame that is not valid JSON, misses `schema_version`, or has an unknown
  top-level shape is rejected with an `Error` frame; the connection stays open.
- **Size limit: 1 MiB per frame** (`MAX_FRAME_BYTES`, measured in 0-C test 3).
  Any frame exceeding it is discarded up to and including the next newline, an
  `Error`/`payload_too_large` frame is returned, and the connection stays
  usable. If a single oversized frame exceeds the **discard cap of 64 MiB**
  the server sends `Error`/`frame_too_large` and closes the connection.
  A 10 MiB frame was measured to be rejected with the connection surviving.
- A single unterminated frame may arrive in any number of `write()` calls; the
  receiver must buffer up to the limit and must not assume syscall boundaries
  carry meaning.

## 2. Message types

Client → server:

| type | discriminator | meaning |
|---|---|---|
| `Event` | presence of `event_id` | one recorded event (see §3) |
| `Ping` | `"type":"Ping"` and no `event_id` | flush barrier and liveness probe |

Server → client:

| type | fields | meaning |
|---|---|---|
| `Ack` | `event_id`, `sequence` | best-effort per-event acknowledgement |
| `Pong` | `nonce`, `watermarks`, `counts` | barrier reached; authoritative delivery state |
| `Error` | `code`, `message` | frame-level rejection, connection stays usable |

**`Ack` is best-effort and may be dropped.** Measured: under a 50-client burst
the server dropped 2 684 of 22 390 acks (12.0 %) because the per-connection reply queue
(bounded, 8 192) was full and the client was not reading. This is deliberate —
an unread ack must never stall ingest — but it means **`Ack` must never be used
as a delivery guarantee.** Only `Pong.watermarks` / `Pong.counts` may be.

`Ping` is answered only after every event received earlier on the same
connection has been ingested (it travels through the same bounded ingest queue),
so a `Pong` is a genuine flush barrier.

## 3. Event envelope (fixed, from `docs/00-phase0-protocol.md` §2)

| field | type | rules |
|---|---|---|
| `schema_version` | integer | `1`; any other value → `Error`/`schema_version_unsupported` |
| `event_id` | string, UUIDv4/v7 | **deduplication key, globally unique**; see §7 |
| `source` | string | `shell.bash&#124;shell.zsh&#124;shell.fish&#124;script&#124;pty&#124;proc&#124;fs&#124;recovery&#124;policy` |
| `session_id` | opaque string | stable for one shell/pty/script lifetime; **not** globally unique across reboots, so it must be paired with a boot id or start timestamp if it is ever persisted |
| `sequence` | u64 | per-`session_id` counter, starts at 0, +1 per event, **never reused within a session** |
| `ts_mono_ns` | u64 | `CLOCK_MONOTONIC` nanoseconds |
| `ts_wall` | string | RFC 3339 UTC with milliseconds, `...Z` |
| `type` | string | open vocabulary; the server must not reject unknown types |
| `payload` | object | type-specific |
| `evidence_status` | string | `observed&#124;declared&#124;inferred` |

`type` is an **open string**, not an enum: an unknown `type` must be stored, not
rejected. During 0-C an early attempt to treat `type` as a closed serde tag
caused every event to be rejected with `unknown_frame_type`; the discriminator
for "is this an event" was therefore changed to "has `event_id`".

## 4. Error codes

| code | direction | cause | connection |
|---|---|---|---|
| `payload_too_large` | server→client | frame > 1 MiB | stays open |
| `frame_too_large` | server→client | oversized frame exceeded the 64 MiB discard cap | closed |
| `invalid_json` | server→client | frame is not parseable JSON | stays open |
| `invalid_envelope` | server→client | JSON but missing/ill-typed envelope fields | stays open |
| `missing_type` | server→client | neither `event_id` nor `type` present | stays open |
| `unknown_frame_type` | server→client | has `type` but no `event_id` and not `Ping` | stays open |
| `schema_version_unsupported` | server→client | `schema_version != 1` | stays open |
| `peer_uid_rejected` | server→client | peer uid not in the allow-list | closed by the server |

Clients must treat any unrecognised `code` as "this frame was not stored" and
must not retry it in a loop.

## 5. Version negotiation

v0 has **no negotiation**: the server accepts `schema_version == 1` and rejects
everything else per frame. Consequences for Phase 1:

- A major protocol change needs a **new socket path** or a `Hello`/`HelloAck`
  exchange; the current design cannot mix versions on one socket.
- The server should advertise its version somewhere a client can read before
  sending events (planned: `Pong` carries `server_schema_version`). Not
  implemented in 0-C, therefore `TBD`.
- Minor additions (new `type` values, new payload keys) are safe because `type`
  is open and unknown `payload` keys must be ignored.

## 6. Timeouts and blocking behaviour

Measured in 0-C test 4 with the daemon `SIGSTOP`ped:

| quantity | measured |
|---|---|
| `SO_SNDBUF` as reported by `getsockopt` | 212 992 B |
| frames accepted before `EAGAIN` (387 B frames) | 167 (65 077 B) |
| frames accepted before `EAGAIN` (8 459 B frames) | 23 (194 583 B) |
| frames accepted before `EAGAIN` (267 B frames) | 167 (45 037 B) |
| buffer drained while the daemon stayed stopped | never (waited 2 s) |
| blocking `write()` with `SO_SNDTIMEO = 2 ms` | returned after 2.3–2.8 ms |
| blocking `write()` with `SO_SNDTIMEO = 50 ms` | returned after 50.6 ms |

Two things follow, and they are mandatory for Phase 1:

1. **The usable buffer is small and is charged per-skb, not per-byte.** 167
   small frames fit, which is far less than `SO_SNDBUF / frame_size` ≈ 550,
   because each frame costs `sizeof(struct sk_buff)` overhead. A client must
   assume **on the order of 10² small events or 10¹ medium events** before the
   socket blocks — never "thousands".
2. **Every client write must be non-blocking or carry an `SO_SNDTIMEO` of a few
   milliseconds.** With a stopped daemon a plain blocking `write()` never
   returns; `SO_SNDTIMEO` is what turns a wedged daemon into a dropped event
   plus a logged error instead of a hung shell.

Recommended client policy: `SOCK_NONBLOCK` + `poll(POLLOUT, 0..2 ms)`; on
`EAGAIN`, drop the event, count it, and advance `sequence` (see §7, §8).

## 7. Deduplication semantics

- The dedup key is **`event_id` only** (`event_id` alone, not
  `(session_id, sequence)`).
- Measured (test 7): a byte-identical resend of an already-accepted event is
  detected, **not** journalled a second time, and counted in `dedup_dropped`.
  A resend of the same `sequence` with a new `event_id` is accepted — correct,
  because a session may legitimately restart its sequence.
- Dedup state is **in-memory and bounded**. In 0-C it is an unbounded
  `HashSet`; Phase 1 must replace it with a bounded LRU plus a persistence
  strategy, otherwise a long-lived daemon grows without limit. Window size:
  `TBD`.
- Consequence: dedup is exact only within the daemon's lifetime and window.
  Across a restart, duplicates reappear as extra rows; consumers must therefore
  also be idempotent on `(session_id, sequence, event_id)`.

## 8. Sequence numbers and loss detection

- `sequence` is assigned **by the client**, per `session_id`, starting at 0,
  monotonic, and **is advanced even when the event is dropped**.
- This is the whole loss-detection mechanism: the receiver computes
  `max(sequence) + 1 - count(rows)` per session. Measured (test 8): with 10 000
  events and a mid-stream `SIGKILL`, the journal audit reported exactly the
  gaps the client had counted as drops, and **zero unexplained gaps**; transport
  loss (written but not journalled) was 0 in both variants.
- A gap therefore means "the client decided not to send this", not
  "the transport lost it". If the product wants to distinguish the two, the
  client must report its drop counter out-of-band (planned `DropReport`, `TBD`).
- The server treats a sequence jump as **normal** and journals it; it must not
  reject or reorder around gaps. In 0-C a gap increments `seq_violations`, which
  is a diagnostic counter, not an error.

## 9. Reconnection

Measured (test 6), client backoff `10, 20, 40, 80, 160, 320, 500, 500 ms`:

| event | measured time |
|---|---|
| `kill -9` → `connect()` starts failing with `ECONNREFUSED` | immediately |
| 8 consecutive failed attempts consumed | 1 130 ms |
| daemon restart → `connect()` succeeds | 1 631 ms after the kill (≈ 1 100 ms of that is the experiment restarting the daemon) |
| worst-case delay to notice a restart | ≤ the backoff ceiling (500 ms here) |

Semantics:

- `connect()` returning success does **not** mean "accepted". The server may
  reject on `SO_PEERCRED` immediately after `accept()`. A client must treat a
  closed connection with `Error`/`peer_uid_rejected` as terminal, not retry it.
- Events produced while disconnected are lost unless the client buffers them
  (bounded, see §10). Whether to buffer and for how long is a product policy;
  measured evidence says a 256-event buffer costs nothing and absorbs ~1 s of
  outage, while a 16-event buffer loses ~6 % of a 10 000-event stream.
- Backoff must be exponential with a ceiling. A fixed ceiling of 500 ms gives
  ≤ 500 ms detection latency; a ceiling of 5 s is acceptable only if the
  product can tolerate 5 s of lost events.
- Do not retry with an unbounded loop and no ceiling: an unattended client
  produced 200+ attempts in 120 s in this experiment before the log was bounded.

## 10. Backpressure and client buffering

- The server's ingest queue is **bounded** (default 1 024, measured at 64 in
  test 9). When full, the reader task stops reading, which stops draining the
  socket, which eventually blocks the client — this is the intended chain.
- The client must have its own bounded queue and an explicit drop policy. In 0-C
  the queue was bounded and `max_queue_depth` never exceeded `queue_cap`
  (measured: cap 64 → depth 64; cap 8 → depth 8), with drops counted in
  `dropped_client_queue_full`.
- Latency under backpressure is large and must be expected: with a 3 ms
  per-event server, the round-trip barrier took 1.11–1.59 s. Any client-side
  timeout below that will fire spuriously during sustained load.
- A client must **never** grow its queue without bound. That is the difference
  between "we lost 597 events and know it" and "the shell was killed by the OOM
  reaper".

## 11. Peer authentication

- The server calls `SO_PEERCRED` on the accepted fd and gets `{pid, uid, gid}`.
  Measured: the values are the **host** uid/gid even when the client runs inside
  a user namespace as a mapped uid (a `unshare --user` client mapped to 5 was
  seen by the server as uid 100 000). This is what makes an allow-list safe.
- Policy: accept only `uid == <expected uid>`; anything else gets
  `Error`/`peer_uid_rejected` and is disconnected before a single frame is
  ingested. Verified: a foreign-uid client wrote 20 events, 0 reached the
  journal.
- `pid` is advisory: it identifies the process at accept time and can exit
  immediately. Do not use it as an identity, only as a diagnostic.
- Filesystem permissions (`0700` dir + `0600` socket) are the first line of
  defence; `SO_PEERCRED` is the second. Neither replaces the other: the
  experiment had to widen the modes to `0755`/`0666` to test the credential
  path at all.

## 12. Durability expectations

- The server journals `(session_id, sequence, event_id, ts_mono_ns)` with a
  `flush()` per line, so a `SIGKILL` loses at most the line in flight.
- Server-side **in-memory counters do not survive a restart.** After `kill -9`
  and restart, `Pong.counts` reports only the post-restart window. The journal,
  not the counter, is the authoritative record — this was a concrete source of
  reconciliation error in 0-C test 8 and must be designed for in Phase 1.

## 13. Open questions (not resolved by 0-C)

| # | question | why it matters |
|---|---|---|
| 1 | dedup window size and persistence | unbounded growth vs. duplicate rows after restart |
| 2 | `SOCK_SEQPACKET` vs `SOCK_STREAM` | message boundaries for free, but smaller buffers; not measured |
| 3 | per-connection vs per-session identity | one shell may open several connections over its lifetime |
| 4 | `DropReport` frame so the server can distinguish client drops from transport loss | today a gap is ambiguous |
| 5 | socket credential/`SO_PASSCRED` vs `SO_PEERCRED` for mid-stream identity changes | `SO_PEERCRED` is sampled once at accept |
| 6 | behaviour under `nofile` exhaustion (client limit is 1 024 on the test host) | a 50-client burst is fine, 1 000 concurrent is not measured |
