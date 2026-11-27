# Phase 3 (`li-helper`), Phase 5 (`BtrfsProvider`/`LvmProvider`), and Phase 6 Report

Project: `linux-intelligence`. Date: 2026-11-27. Branch: `master`.
Scope: User-authorized completion of `PLAN.md §8.1` gates (`D1` privileged helper core, `D2` sandboxed `/tmp` Btrfs image & host-probed LVM recovery providers, `D5` ADR & v0 spec status acceptance, `D8` retaining `synchronous=NORMAL`) and Phase 6 packaging/capability probing deliverables.

## 1. What was actually completed

| Area | Status | Evidence / Delivery |
|---|---|---|
| Phase 3 Privileged Helper (`P3-06..09`, `D1`) | Completed | `crates/li-daemon/src/helper.rs`, `crates/li-daemon/src/bin/li-helper.rs`, `crates/li-daemon/tests/helper.rs`: zero-`execve` core, `PR_SET_NO_NEW_PRIVS`, closed numeric opcodes (`1..3`, opcode `4` disabled by design), `openat2(RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS \| RESOLVE_NO_MAGICLINKS)`, `SO_PEERCRED` UID enforcement, 64 KiB / 8 MiB framing, 2 s half-frame timeout (`P3-08`), 64-client semaphore, dense `0600` JSONL audit log labeled `mode_0600_owner_not_tamper_proof` (`P3-09`) |
| Phase 5 `BtrfsProvider` & `LvmProvider` (`P5-04..05`, `D2`) | Completed (sandbox / probe) | `crates/li-daemon/src/recovery.rs`, `crates/li-daemon/tests/recovery.rs`: `/tmp` sandbox `BtrfsProvider` using `mkfs.btrfs -q -r` + `btrfs inspect-internal dump-super` + unprivileged `btrfs restore -o` without touching host subvolumes or `mount -o loop`; `LvmProvider` probes `/usr/bin/{lvs,lvcreate}` and degrades explicitly when absent |
| Phase 6 POSIX Capability Probe & Systemd Unit (`P6-01..08`) | Completed (host Arch x86_64) | `packaging/probe.sh` (strict POSIX `sh`, `cat` on procfs, `/proc/1/comm == systemd`), `packaging/systemd/li-daemon.service`, `packaging/README.md` |
| ADR & Spec Finalization (`D5`, `P6-09`) | Completed | `docs/adr/README.md`, `docs/adr/0001..0006.md`, `docs/ipc-protocol-v0.md`, `docs/security-boundary-v0.md` promoted from `proposed`/`draft` to `accepted` with implemented deviations documented inline; `docs/PLAN.md` and `README.md` updated |

Not completed / explicitly `NOT TESTED` on this host:
- `P6-05`: Cross-compilation or live testing on `ARM64`, `x86_64-unknown-linux-musl` / Alpine, or NixOS (`NOT TESTED`: system uses distro `rustc` without `rustup`/musl targets).
- `P6-06`: Host procfs mounted with `hidepid` (`NOT TESTED`: `/proc` on host has `proc_hidepid: "absent"`).
- Real privileged `btrfs subvolume snapshot` / `mount -o loop` recovery (`EPERM` without `CAP_SYS_ADMIN`; avoided intentionally per `D2`).

## 2. Files changed

| File | Purpose |
|---|---|
| `crates/li-daemon/Cargo.toml` | Added `serde` workspace dependency and `thread` feature on `rustix` |
| `crates/li-daemon/src/{lib.rs,helper.rs,bin/li-helper.rs}` | `li-helper` hardened library module and binary entrypoint |
| `crates/li-daemon/tests/helper.rs` | 6 integration/security boundary tests for `li-helper` |
| `crates/li-daemon/src/recovery.rs`, `crates/li-daemon/tests/recovery.rs` | `BtrfsProvider` (sandbox image), `LvmProvider` (capability probe), `ProviderRegistry::with_defaults`, failure-audit verification |
| `packaging/{probe.sh,README.md,systemd/li-daemon.service}` | POSIX capability probe, user systemd unit, static-vs-dynamic notes |
| `docs/adr/{README.md,0001..0006.md}`, `docs/{ipc-protocol-v0.md,security-boundary-v0.md,PLAN.md}`, `README.md` | Truthful ADR/spec acceptance (`D5`) and project status synchronization |
| `docs/reports/PHASE6-REPORT.md` | This completion report |

## 3. Test results

All verification commands ran offline after the final code and doc edits:

1. `cargo fmt --check` — passed (0 diffs).
2. `cargo clippy --offline --workspace --all-targets -- -D warnings` — passed (0 warnings, 0 errors).
3. `cargo test --offline --workspace --all-targets` — **155 passed, 0 failed, 0 ignored**:
   - `li-cli`: 3 unit + 1 integration (`cli_daemon`) = **4 passed**
   - `li-daemon`: `correlation` 4 + `daemon` 6 + `e2e` 2 + `helper` 6 + `observers` 4 + `pty` 3 + `recovery` 3 = **28 passed**
   - `li-hook`: 5 unit + 9 integration (`hook`) = **14 passed**
   - `li-journal`: 12 unit + 7 `concurrency` + 1 `crash` + 7 `migrate` + 5 `redaction` + 13 `schema` = **45 passed**
   - `li-proto`: 13 unit + 51 `protocol` = **64 passed**
4. `cargo build --offline --release` — built cleanly in 47.48s (`target/release/{li,li-daemon,li-helper,li-hook}`).
5. `sh -n packaging/probe.sh && sh packaging/probe.sh` — exited 0 (`kernel_release: "7.3.5-zen1-1-zen"`, `systemd_is_pid1: true`, `libc: "glibc 2.44"`, `btrfs: true`, `mkfs_btrfs: true`, `lvs: false`, `lvcreate: false`, `dash: false`).
6. `python3 shell/test_matrix.py --hook "$PWD/target/debug/li-hook" -v` — `Ran 1 test in 123.440s OK` (39/39 shell matrix cases across bash, zsh, and fish).

## 4. Architecture changes / deviations

- **`BtrfsProvider` sandbox mode**: Instead of running `mount -o loop` (fails with `EPERM` without `CAP_SYS_ADMIN`) or operating on host root/home Btrfs subvolumes (`D2`), `BtrfsProvider` uses `mkfs.btrfs -q -r` + `btrfs inspect-internal dump-super` + userspace `btrfs restore -o` inside `/tmp`.
- **`li-helper` / `D1`**: `OPCODE_BTRFS_SNAPSHOT` (`4`) returns `not_implemented_by_design` rather than shelling out, preserving the invariant of 0 `execve` calls after startup.
- **`dash` absent on host**: Arch Linux host has `/bin/sh -> /usr/bin/bash` and no `dash` installed; `packaging/probe.sh` passes `sh -n` and follows strict POSIX (`cat` procfs, no bashisms).

## 5. Known problems

1. `ARM64`, `x86_64-unknown-linux-musl` / Alpine, NixOS, and `hidepid` `/proc` mounts remain `NOT TESTED` locally.
2. `lvs` / `lvcreate` are absent on this host, so only `LvmProvider` capability degradation (not live LVM snapshot creation) executed here.
3. Dynamic release binaries compiled on Arch (`glibc 2.44`) are not portable to older `glibc` distributions per 0-F H4; CI `musl` builds are required for portable distribution binaries.
4. Helper `0600` audit logs do not prevent tampering by the owning UID (`mode_0600_owner_not_tamper_proof`, P3-09).

## 6. Security risks

| Risk | Status | Basis |
|---|---|---|
| Helper shell/command injection or `exec` reachability | mitigated | Closed numeric opcodes (`1..3`), strict key checks, zero `Command::new`/`execve` verified in `tests/helper.rs` |
| Path traversal / symlink escape in helper | mitigated | Lexical whitelist + `openat2(RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS \| RESOLVE_NO_MAGICLINKS)` tested against 8 escapes & 12 metachars |
| Helper half-frame exhaustion (`P3-08`) | mitigated | 2 s partial-frame timeout + 64-connection semaphore verified in `tests/helper.rs` |
| Audit log tampering by owning UID (`P3-09`) | open (by design) | Explicitly documented as `mode_0600_owner_not_tamper_proof`; no false tamper-evidence claim |
| Host filesystem damage during Btrfs testing | mitigated | Zero host subvolumes touched; all Btrfs tests operate on scratch `.img` files in `/tmp` |

## 7. Next phase plan

All planned Phase 0–6 implementation deliverables on the local Arch x86_64 host are complete. Any future CI pipeline should add `x86_64-unknown-linux-musl` static builds, `ARM64` runners, and `dash` / `hidepid` container checks.

## 8. Git status

Branch `master`, baseline before final Phase 1–6 completion commit: `c6d62d7` (ready to commit cleanly via `git commit`).
