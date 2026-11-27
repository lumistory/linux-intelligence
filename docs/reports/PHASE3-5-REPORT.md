# Phase 3 (Non-Privileged), Phase 4, and Phase 5 (Scratch Providers) Report

Project: `linux-intelligence`. Date: 2026-11-27. Branch: `master`.
Scope: Autonomous unprivileged milestones per `docs/PLAN.md` §8.2 — Phase 3 PTY supervisor (P3-01..05), Phase 4 Deduplication & R0–R7 Correlation Engine (P4-01..10), and Phase 5 `RecoveryProvider` + `file`/`git` scratch recovery (P5-01..03, P5-06).

Privileged helper (`D1`), real-system destructive restore (`D2`), ADR status flip (`D5`), `synchronous=FULL` (`D8`), and Phase 6 cross-distro packaging are intentionally stopped at `PLAN.md §8.1` decision gates.

## 1. What was actually completed

| Area | Status | Evidence / Delivery |
|---|---|---|
| Phase 3 PTY supervisor (P3-01..05) | Completed (non-privileged) | `crates/li-daemon/src/pty/{mod.rs,responder.rs}`: `rustix::pty` + `/usr/bin/setsid --ctty` (zero `unsafe`), DSR/DA1/DA2/kitty/OSC 10-11/`ESC[18t`/DECRQM/XTVERSION/XTGETTCAP responder, `fish` fast startup, 4-session concurrency, `resize`, explicit foreign-terminal distinction, loud failure on missing zsh module dir |
| Phase 4 Correlation Engine (P4-01..10) | Completed | `crates/li-daemon/src/correlation/{mod.rs,confidence.rs,relations.rs,reorder.rs}` + `li correlate`: two-level dedup (never deletes rows), clock-based `Reorderer` (`LateArrival`), R0–R7 total classifier, `RelationKind` rejecting `caused_by`/`causes`/`led_to`/`result_of`, many-to-many `operations`/`event_operations`/`event_relations`/`evidence`, file attribution capped at `Probable` (`Unknown` + `path_not_tied` when `R3` attempted) |
| Phase 5 Recovery (P5-01..03, P5-06) | Completed (scratch-only) | `crates/li-daemon/src/recovery.rs`: `RecoveryProvider` trait + `ProviderRegistry` (refuses empty `caveats`), `FileBackupProvider`, `GitProvider`, `execute_restore` (`preview -> overwrite_check -> scope_hash approval -> restore -> verify`, logging `ApprovalGranted`/`RecoveryStarted`/`RecoveryCompleted` on success & failure) |

## 2. Files changed

- `crates/li-daemon/src/pty/mod.rs`, `crates/li-daemon/src/pty/responder.rs`, `crates/li-daemon/tests/pty.rs`
- `crates/li-daemon/src/correlation/{mod.rs,confidence.rs,relations.rs,reorder.rs}`, `crates/li-daemon/tests/correlation.rs`
- `crates/li-daemon/src/recovery.rs`, `crates/li-daemon/tests/recovery.rs`
- `crates/li-cli/src/main.rs`, `crates/li-cli/Cargo.toml`
- `docs/PLAN.md`

## 3. Test results

Verified after the final edit via `cargo fmt --check`, `cargo clippy --offline --workspace --all-targets -- -D warnings`, and `cargo test --offline --workspace --all-targets`.

## 4. Architecture changes / deviations

- Implemented Phase 4 correlation and Phase 5 providers as `li-daemon` library modules (`correlation`, `pty`, `recovery`) wired into `li-cli`, maintaining the 5-crate workspace layout (`P1-27`) and strict `unsafe_code = "forbid"`.
- Used `/usr/bin/setsid --ctty` with `rustix::pty::ioctl_tiocgptpeer` instead of `unsafe { CommandExt::pre_exec }` for zero-`unsafe` PTY session isolation.

## 5. Known problems

1. Phase 3 privileged helper (`P3-06..09`) requires user sign-off (`D1`).
2. Phase 5 `btrfs`/`snapper` and `LVM` providers (`P5-04`, `P5-05`) require dedicated scratch volumes and were not tested.
3. Phase 6 (`ARM64`, `musl/Alpine`, `hidepid`, systemd packaging) is unstarted.

## 6. Security risks

| Risk | Status | Basis |
|---|---|---|
| Foreign terminal hijacking | mitigated | `PtyCapabilities::foreign_terminal()` explicitly disables `TIOCSTI`/takeover |
| Causality overclaim / file attribution upgrade | mitigated | `RelationKind::parse` rejects causal verbs; `classify` caps file attribution (`tests/correlation.rs`) |
| Unapproved or unverified restore | mitigated | `execute_restore` enforces matching `scope_hash` approval and logs `RecoveryCompleted` failures |

## 7. Next phase plan

Await user confirmation on `PLAN.md §8.1` gates (`D1` privileged helper, `D2` btrfs/LVM scratch target, `D5` promoting ADRs to `accepted`, `D8` `synchronous=FULL` vs `NORMAL`) and Phase 6 packaging scope.

## 8. Git status

Branch `master`, uncommitted working tree.
