# Phase 2 Completion Report

Project: `linux-intelligence`. Date: 2026-11-27. Branch: `master`.
Scope: System Observers (`procfs` 200 ms poller, non-blocking `inotify` watcher, 0-E §8.2 capability declaration, eBPF necessity verdict P2-10).

## 1. What was actually completed

| Task | Status | Evidence / Delivery |
|---|---|---|
| P2-01 `procfs` 200 ms poller | Completed | `crates/li-daemon/src/observers/procfs.rs` (`Stat` depth default, `cmdline` on creation, `DEFAULT_PROC_POLL_INTERVAL = 200ms`) |
| P2-02 `<60ms` process blind spot | Completed | Published verbatim in `observers/mod.rs::system_observer_capabilities` |
| P2-03 `(pid, starttime)` + collision flag | Completed | `ProcPoller` tracks `(pid, starttime)` incarnations and sets sticky `ambiguous=1` on reuse; verified 20/20 trials in `tests/observers.rs` |
| P2-04 `inotify` 64 KiB + `IN_Q_OVERFLOW` | Completed | `crates/li-daemon/src/observers/fs.rs`: `rustix::fs::inotify` with 64 KiB buffer, `rustix::event::poll`, `InotifyOverflow` event + `rescan_trees()` |
| P2-05 Auto-add new subtree watches | Completed | `add_subtree` on `CREATE | ISDIR` / `MOVED_TO | ISDIR` (`inotify_watcher_auto_watches_subdirs_and_never_attributes_writer`) |
| P2-06 Unattributed `Observed` file events | Completed | Emits `FileCreated`/`FileModified`/`FileDeleted`/`FileRenamed` with `EvidenceStatus::Observed` and zero `pid`/`uid` fields |
| P2-07 Verbatim 0-E §8.2 capability record | Completed | Persisted to `adapter_registry` under `adapter = 'system-observer'` |
| P2-10 eBPF necessity determination | Completed | Verdict: optional for Phase 3+ privileged escape hatch (D1) only; Phase 2 remains zero-privilege |

Not executed in this pass: 60-second continuous soak measurement for live RSS/CPU inside `li-daemon` (host scan single-pass latency and 0-E §4 A4 3.93% / 2.7 MB baseline apply), and P2-08/P2-09 multi-day clock skew soak.

## 2. Files changed

- `crates/li-daemon/src/observers/mod.rs`
- `crates/li-daemon/src/observers/procfs.rs`
- `crates/li-daemon/src/observers/fs.rs`
- `crates/li-daemon/src/lib.rs`, `src/main.rs`, `Cargo.toml`
- `crates/li-daemon/tests/observers.rs`

## 3. Test results

- `cargo clippy --offline --workspace --all-targets -- -D warnings`: exit 0.
- `cargo test --offline -p li-daemon`: **12 passed, 0 failed** (`daemon` 6, `e2e` 2, `observers` 4 — including 18,500-file real `IN_Q_OVERFLOW` trigger and 20/20 PID collision test).

## 4. Architecture changes / deviations

- Used `rustix::fs::inotify::Reader` + `rustix::event::poll` on a dedicated blocking thread instead of `tokio::io::unix::AsyncFd::register` (which is `unsafe` in Tokio 1.54) or `nix` (hardcoded 4 KiB buffer), keeping `unsafe_code = "forbid"` intact and honoring the `>= 64 KiB` buffer constraint.

## 5. Known problems

1. Unprivileged `inotify` cannot attribute file writes to `pid`/`uid` and loses events between `mkdir` and `add_watch` (inherent kernel ABI boundary, declared in `known-lossy`).
2. `hidepid` hosts remain `NOT TESTED`.

## 6. Security risks

| Risk | Status | Basis |
|---|---|---|
| False file->process attribution | mitigated | C4 enforced structurally (`pid`/`uid` never emitted on fs events) |
| Silent `IN_Q_OVERFLOW` data loss | mitigated | Explicit `InotifyOverflow` event + full `rescan_trees()` |
| Same-tick PID reuse misjoin | mitigated | Sticky `ambiguous = 1` + `incarnation_seq` increment |

## 7. Next phase plan

Proceed to **Phase 4 (Deduplication, Reordering & R0–R7 Correlation Engine)** and **Phase 3 non-privileged PTY Supervisor** while keeping privileged helper (`D1`) gated behind user sign-off.

## 8. Git status

Branch `master`, uncommitted working tree.
