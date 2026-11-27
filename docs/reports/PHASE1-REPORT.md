# Phase 1 Completion Report

Project: `linux-intelligence`. Date: 2026-11-27. Branch: `master`.
Scope: Phase 1 end-to-end infrastructure (`li-proto`, `li-journal`, `li-hook`, `li-daemon`, `li-cli`, `shell/`).

## 1. What was actually completed

| Task | Status | Evidence / Delivery |
|---|---|---|
| P1-01..06 Base crates | Completed | `crates/li-proto` (64 tests), `crates/li-journal` (12 tables, 23 indexes, 2 append-only triggers, single writer, pre-insert redaction, `SIGKILL` crash test) |
| P1-07..09 `li-hook` transport | Completed | `crates/li-hook`: `rustix` `SO_SNDTIMEO=5ms` on inherited stdout FD and `--sink=socket`, bounded queue, sequence advance on drop, `DropReport` on live sink exit |
| P1-10 `panic="abort"` | Cancelled by decision | Cargo forbids package-level `panic="abort"`; kept panic-free input handling without changing workspace profile |
| P1-11..15 `li-daemon` Tokio rewrite | Completed | `crates/li-daemon`: Tokio async UDS (`0700`/`0600`), `SO_PEERCRED` UID gate, 2 s partial-frame timeout, `MAX_FRAME_BYTES` enforcement, `journal.db` persistent path, automatic `sessions` FK + `adapter_registry` registration, `ts_mono_ns` stamping, real `Pong` watermarks, async writer backpressure, `boot_id` + `DaemonRestarted` reconciliation |
| P1-16..19 Shell adapters | Completed | `shell/{bash,zsh,fish}` + `capabilities/*.json` (consolidated from `adapters/`); fixed bash exit-code preservation in `_li_finish` and `test_matrix.py` child FD close |
| P1-20..25 `li-cli` | Completed | `crates/li-cli`: `li run` (`setsid --wait` + leader record + `/proc` lineage, distinct `ScriptStarted`/`ScriptFinished`, `internal_script_commands: false`), `li query` (`evidence_status` + `credibility_level`, hard `"caused"` guard), `li checkpoint` (`manual`/`git`, non-destructive with `caveats`), `li purge`, `li status`, `li ping`, `li shell-init` |
| P1-26 Audited purge | Completed | `Journal::purge_before` in single `BEGIN IMMEDIATE` transaction (drop/recreate triggers + `EventsPurged` audit row) |
| P1-27 Crate split | Deferred by decision | Kept 5 crates for Phase 1; `li-recovery` / `li-policy` deferred to Phase 4/5 |
| P1-29 Verification | Completed (with A5 note) | 135 Rust tests + `clippy -D warnings` + `fmt --check` + 39/39 PTY shell matrix + 10,000-event burst test + CLI-to-daemon UDS E2E test |

Not done in Phase 1: `P1-28` (`$.new_path` index, deferred to v3 rename queries) and a 10-minute wall-clock run of A5 (replaced in automated CI by the paced 10,000-event burst test).

## 2. Files changed

See `git status --short` (`li-proto`, `li-journal`, `li-hook`, `li-daemon`, `li-cli`, `shell/`, `README.md`, `docs/PLAN.md`). Duplicate `adapters/` directory removed.

## 3. Test results

- `cargo fmt --check`: exit 0.
- `cargo clippy --offline --workspace --all-targets -- -D warnings`: exit 0 (0 warnings).
- `cargo test --offline --workspace --all-targets`: **135 passed, 0 failed** (`li-proto` 13+51, `li-journal` 12+7+1+7+5+13, `li-hook` 5+9, `li-daemon` 6+2, `li-cli` 3+1).
- `python3 shell/test_matrix.py --hook "$PWD/target/debug/li-hook" -v`: exit 0, 39/39 cases passed in 123.54 s.

## 4. Architecture changes / deviations

- Consolidated `adapters/` into `shell/` to eliminate duplicated scripts and stale capability docs.
- Replaced synchronous thread-per-connection `li-daemon` with Tokio async UDS + backpressure retry and moved database storage from `$XDG_RUNTIME_DIR/events.db` to `~/.local/state/linux-intelligence/journal.db`.
- Added `DropReport` client frame so producer drops are distinguished from unexplained gaps at restart.

## 5. Known problems

1. `A5` 10-minute wall-clock soak and `synchronous=FULL` vs `NORMAL` throughput comparison (`D8`) remain `NOT MEASURED`.
2. Production systemd user service and rc-file launcher auto-wiring are deferred until packaging/distribution.
3. Positional secrets on the command line remain structurally undetectable by regex redaction.

## 6. Security risks

| Risk | Status | Basis |
|---|---|---|
| Stalled daemon hangs shell | mitigated | `li-hook` 5 ms `SO_SNDTIMEO` + permanent retirement on failure |
| Dead sink SIGPIPE kills shell | mitigated | Probe + guarded writes in `shell/` (`every_installed_shell_survives_a_dead_sink`) |
| Foreign UID socket access | mitigated | `0700`/`0600` + `SO_PEERCRED` (`peer_uid_mismatch_is_rejected_with_error_frame`) |
| Partial frame DoS (0-G T7) | mitigated | 2 s `PARTIAL_FRAME_TIMEOUT` + 256-client `Semaphore` |
| Secrets persisted in SQLite/WAL | mitigated | Pre-row redaction (`tests/redaction.rs` scans `.db` and `-wal` raw bytes) |

## 7. Next phase plan

Proceed to **Phase 2 (System Observers)** in `li-daemon`:
1. `procfs` 200 ms poller (`(pid, starttime)` + `incarnation_seq` + sticky `ambiguous`, `ProcessCreated`/`ProcessExited`).
2. `inotify` non-blocking watcher (>= 64 KiB read buffer, recursive auto-watch, `IN_Q_OVERFLOW` subtree rescan, unattributed `Observed` file events).
3. Verbatim 0-E §8.2 capability publication and threshold verification (CPU <= 5%, RSS <= 10 MB).

## 8. Git status

Branch `master`, uncommitted working tree containing Phase 1 closure.
