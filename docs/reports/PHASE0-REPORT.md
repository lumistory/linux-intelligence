# Phase 0 — Completion Report

Project: `linux-intelligence` — Linux System Observability & Operation Recovery
Infrastructure.
Date: 2026-11-23. Branch `master`, 11 commits, 1786 tracked files.
Protocol: `docs/00-phase0-protocol.md` (8-section report format, no fabricated
results, `NOT TESTED` for anything unmeasured).

---

## 1. What was actually completed

Eight experiments, all executed on this machine, all with raw logs retained:

| Exp | Subject | Verdict | Evidence |
|---|---|---|---|
| 0-A | bash/zsh/fish interactive hooks | **PASS-WITH-LIMITS** | 39/39 cases (13×3), hook-body cost 3 µs (bash) / 2.3 ms (zsh) / 1.1 ms (fish) |
| 0-B | Non-interactive script lifecycle | **PASS-WITH-LIMITS** | 41/41 assertions, 10 cases |
| 0-C | UDS concurrency and reliability | **PASS** | 10/10 tests, 3 full repeat runs |
| 0-D | PTY across shells | **PASS** | 49/49 assertions, 30/30 steps, 4 concurrent sessions |
| 0-E | procfs and inotify | **PASS-WITH-LIMITS** | 20 tests, 2 hypotheses falsified |
| 0-F | Arch/Ubuntu/Debian/Fedora | **PASS-WITH-LIMITS** | 4 distros × 21 probe fields, 58 logs |
| 0-G | Privilege model + helper | **PASS** | 29/29 checks, 5 defects found and fixed in the driver/helper |
| 0-H | Dedup/correlation/context | **PASS** | 24 credibility scenarios, 100k-row bench |

Reports total 3 485 lines. Supporting documents: 6 ADRs (`docs/adr/`),
`docs/ipc-protocol-v0.md`, `docs/security-boundary-v0.md`,
`docs/spec/phase1-implementation-spec.md`, `docs/reports/00-environment-audit.txt`.

## 2. Files changed

| path | purpose |
|---|---|
| `docs/00-phase0-protocol.md` | the experiment contract every worker had to follow |
| `docs/00-subagent-brief.md` | shared rules for parallel workers |
| `docs/adr/0001..0006` | language/runtime, IPC, storage, shell boundary, privilege, recovery |
| `docs/ipc-protocol-v0.md` | wire format, message types, limits, dedup, reconnect |
| `docs/security-boundary-v0.md` | trust boundary, identity triple, allowlist principles |
| `docs/spec/phase1-implementation-spec.md` | Phase 1 plan constrained by measured facts |
| `experiments/00-*/` | 8 experiments: README, run.sh, src, results, REPORT.md |
| `experiments/toolchain/root/` | locally unpacked zsh 5.9.2 (gitignored) |

## 3. Test results

Individual suites (all `run.sh`, exit 0):

| suite | result |
|---|---|
| 0-A | 39/39 cases executed; 12 latency modes × 180 samples |
| 0-B | 41 assertions pass, 0 fail; `cargo test` 7/7; clippy clean |
| 0-C | 10/10 tests; 3 consecutive full runs identical on every structural metric |
| 0-D | 49 assertions pass; 30/30 shell steps; 4 sessions zero cross-talk |
| 0-E | 7 hypotheses: 4 PASS, 1 PASS-PARTIAL, 2 FAIL (recorded as such) |
| 0-F | 21 fields × 5 environments; 12 SAME (kernel), 9 DIFFERS (user space) |
| 0-G | 29/29 checks; `ALL CHECKS PASSED` |
| 0-H | 7/7 hypotheses; 24/24 credibility assertions |

Failures that were **found and kept in the record** (not deleted, not softened):

- 0-E H2 `(pid,starttime)` is a unique key → **FAIL**, 20/20 same-tick collisions.
- 0-E H6 one `write()` produces multiple `IN_MODIFY` → **FAIL**, it is one per
  syscall.
- 0-A H-A6 a raw write to a dead sink is survivable → **FALSIFIED**, it kills the
  shell.
- 0-G: five real defects found and fixed in the driver/helper — `PR_GET_NO_NEW_PRIVS`
  misuse, oversize frames not audited, a no-newline oversize frame being
  re-enterable (request-smuggling shape), the helper unconditionally rewriting
  its socket parent's permissions (which made the filesystem gate masquerade as
  the allowlist gate), and a `strace` wrapper's pid taken for the helper's pid.
- 0-G: one assertion matched its own source text (T6) — found and fixed.

## 4. Architecture changes

Three decisions were changed *because* of measurement, not preference:

1. **IPC client policy hardened** (ADR 0002). A blocking write with no timeout
   never returned against a SIGSTOPped daemon. Non-blocking or
   `SO_SNDTIMEO ≤ 5 ms` is now mandatory; on `EAGAIN` the client drops, counts,
   and advances `sequence` so the gap is detectable. 0-A additionally proved
   that **no shell can open an AF_UNIX socket by redirection** (ENXIO on bash,
   zsh, fish), so the sink must be a launcher-supplied inherited FD, not a
   shell-opened socket.
2. **Per-event `Ack` demoted** from "delivery proof" to "best effort". 12.0 % of
   acks were dropped under a 50-client burst. Delivery proof is a watermark.
3. **Recovery scope narrowed** (ADR 0006). 0-F measured that no test container
   ships btrfs-progs, snapper or lvm2, and that snapshot/rollback was never
   exercised — so the recovery provider's `caveats` list is now mandatory and
   Phase 1 implements **no** destructive restore.

No core architectural principle was abandoned. The six-layer model, the
two-tier event/context storage, the Provider abstraction and the L0–L4 policy
scale all survived contact with measurement.

## 5. Known problems

Open, and not hidden:

1. **Nothing has been tested against real user shell traffic.** Every event
   population in 0-H is synthetic. The trap classes were designed by the author
   of the experiment, which is exactly the blind spot synthetic data has.
2. **zsh was never tested as a distribution package.** All zsh results depend on
   a `ZDOTDIR` shim into an unpacked tree.
3. **Recovery is the least validated area.** 0-F discovered state but never
   restored anything; no snapshot was created or rolled back.
4. **Scale ceiling is 100 k rows.** No measurement at 1 M.
5. **The digest used for secret-linkability is 64-bit FNV-1a.** Linkable by
   design; not a protection for the secret and not collision-resistant enough to
   be trusted for anything but "same value seen again".
6. **`hidepid` hosts, musl/Alpine, ARM, nushell** all `NOT TESTED`.
7. **Half-frame stall in the helper's framing** (0-G T7) is reproduced and
   documented, not fixed: a frame with no newline and under the discard cap
   blocks that one connection in `read()` (1500 ms timeout measured; new
   connections unaffected).
8. **The audit log's `0600` protects it from other users, not from its owner.**
   Any claim of tamper-evidence is unsupported; marked `NOT TESTED`.

## 6. Security risks

| risk | status |
|---|---|
| A wedged daemon hanging every shell | **mitigated by design**: write timeout plus an inherited FD, so a dead sink and a live one cost the same (bash 1148 µs vs 1140 µs prompt-to-prompt, i.e. inside the noise) |
| A dead sink killing the shell via SIGPIPE | **mitigated** (subshell write); `trap '' PIPE` rejected because it leaks `SigIgn` into children |
| `SO_PEERCRED` pid being reused after the client exits | **demonstrated**; mitigation is to authorise on uid only and treat pid as attribution |
| `(pid,starttime)` colliding within a tick | **demonstrated 20/20**; mitigation is to drop to `Unknown` rather than guess |
| Privilege escalation through the helper | **closed in prototype**: closed numeric opcodes, `openat2(RESOLVE_BENEATH)`, no exec-capable call in the crate (grep-verified), 10/10 injection attempts refused |
| Secrets reaching the journal | **mitigated** for the three measured shapes (0 misses, 1 false positive / 53); a bare positional secret remains undetectable |
| Inotify overflow silently losing events | **not mitigable** at this layer; declared as `partial:IN_Q_OVERFLOW is data loss` |
| Misattributing a file change to a process | **impossible** to do correctly here; inotify carries no pid and non-privileged fanotify is denied |

## 7. Next phase plan

Phase 1 (core infrastructure), in dependency order:

1. `li-proto` — envelope and message types, no I/O, version negotiation.
2. `li-journal` — SQLite schema from migrations 0001, append-only triggers,
   expression index on JSON paths **from the first migration** (0-H measured a
   169× penalty for adding it later).
3. `li-hook` — dependency-light client binary; non-blocking; bounded queue;
   drop counter; `sequence` advance on drop.
4. `li-daemon` — Tokio accept loops, bounded ingest, **single SQLite writer
   thread** (0-C: the flush is the bottleneck, not the socket), crash sentinel,
   restart reconciliation.
5. Shell adapters — bash `PROMPT_COMMAND`+`history 1`, zsh `preexec`/`precmd`,
   fish `fish_preexec`/`fish_postexec`, capability records published verbatim.
6. `li-cli` — `li run`, `li query`, `li checkpoint` (checkpoint creation only,
   no restore).

Acceptance criteria are in `docs/spec/phase1-implementation-spec.md §6`, all
phrased as measurements rather than claims.

**Explicitly not in Phase 1**, per the gates: no privileged helper (0-G defers
it to Phase 3 *at the earliest*, and only if a Phase 2 measurement shows a
capability that cannot be obtained otherwise); no destructive restore (0-F never
exercised snapshot or rollback); no file→process attribution (0-E: inotify
carries no pid, non-privileged fanotify is denied); no eBPF, no kernel audit.

## 8. Git status

- Branch: `master`
- Commits: 11 (`afa8d77` most recent)
- Uncommitted at report time: `docs/security-boundary-v0.md` (modified),
  `experiments/00-g-privilege/REPORT.md` (modified), two 0-H retention logs
  (modified) and one new retention log — all from the last two experiment runs.
- Nothing has been pushed anywhere; this is a local repository only.
