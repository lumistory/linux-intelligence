# Phase 1 - Batch 3 Report

Project: `linux-intelligence`. Date: 2026-11-24. Branch: `master`, HEAD
`77d8a4f`. Scope: P1-16, P1-17, P1-18, P1-19 only. No commit was made.

## 1. What was actually completed

| task | status | evidence / delivery |
|---|---|---|
| P1-16 Bash | implemented | [`shell/bash/bash.sh`](../../shell/bash/bash.sh): `PROMPT_COMMAND` plus builtin `history 1`; PS0 records only the start tick; writes are guarded and fail silent. The hot path contains no command substitution. |
| P1-17 Zsh | implemented | [`shell/zsh/zsh.zsh`](../../shell/zsh/zsh.zsh): `preexec`/`precmd`, direct hook arrays, unpacked-zsh `module_path`/`fpath` handling, six-digit fraction truncation before arithmetic. A real syntax check and matrix run passed. |
| P1-18 Fish | implemented with declared caveat | [`shell/fish/fish.fish`](../../shell/fish/fish.fish): `fish_preexec`/`fish_postexec`, guarded sink writes, inherited FD only. Fish dead-sink resilience remains the required unknown value. |
| P1-19 capability publication | implemented | [`capabilities/bash.json`](../../capabilities/bash.json), [`capabilities/zsh.json`](../../capabilities/zsh.json), [`capabilities/fish.json`](../../capabilities/fish.json) are published in `SessionStarted` payloads and checked for equality by the real matrix. `partial:` values are preserved; explicitly unmeasured fields remain `unknown`. |
| 0-A matrix | passed | `python3 shell/test_matrix.py --hook "$PWD/target/debug/li-hook" -v`: `Ran 1 test ... OK`; the test covers all 13 cases across bash/zsh/fish, i.e. 39/39 combinations. |

The fish capability value `hook_can_kill_shell_if_sink_dies` is retained exactly
as `unknown:fish_is_not_killed_by_SIGPIPE_in_this_setup_untested`. It was not
upgraded based on bash or zsh behavior.

Not completed in this batch: P1-20 and every task after P1-20; no launcher or
daemon ingest path was added; no claim is made that the full Phase 1 acceptance
set A2-A9 is complete. The existing workspace changes for the preceding
`li-hook` work remain uncommitted and were not reverted.

## 2. Files changed

| path | purpose |
|---|---|
| `shell/bash/bash.sh` | Bash interactive adapter, bounded inherited-FD emission and SessionStarted capability record |
| `shell/zsh/zsh.zsh` | Zsh preexec/precmd adapter, module path fix and timestamp handling |
| `shell/fish/fish.fish` | Fish pre/postexec adapter and guarded inherited-FD emission |
| `capabilities/bash.json` | Bash capability record |
| `capabilities/zsh.json` | Zsh capability record |
| `capabilities/fish.json` | Fish capability record, including the required unknown dead-sink field |
| `shell/test_matrix.py` | Real PTY/socket integration matrix for the three product adapters |
| `docs/reports/PHASE1-BATCH3-REPORT.md` | This report |

Line budget, measured after implementation:

| measured files | lines |
|---|---:|
| `shell/bash/bash.sh` | 151 |
| `shell/zsh/zsh.zsh` | 156 |
| `shell/fish/fish.fish` | 129 |
| `shell/test_matrix.py` | 181 |
| `capabilities/*.json` | 3 |
| **batch code/test/config total** | **620** |
| allowed maximum | 1200 |

The batch total is 620 lines, 580 below the limit, and did not exceed the
1200-line stop condition. The report itself is excluded from that code budget.

## 3. Test results

All commands below were actually run in `/home/zou/linux-intelligence`.

| command | actual result |
|---|---|
| `bash -n shell/bash/bash.sh && zsh -n shell/zsh/zsh.zsh && fish -n shell/fish/fish.fish` | exit 0 |
| `grep -c '\$(' shell/zsh/zsh.zsh` | `0`; the zsh adapter contains no `$( )` command substitution |
| `cargo fmt --check` | exit 0, no output |
| `cargo check --workspace --all-targets` | exit 0, `Finished dev profile` |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, `Finished dev profile` |
| `cargo test --workspace --all-targets` | exit 0; 128 passed, 0 failed across the reported test binaries |
| `python3 shell/test_matrix.py --hook "$PWD/target/debug/li-hook" -v` | exit 0; `Ran 1 test ... OK`; 39/39 shell/case combinations exercised |

The matrix emitted Python `ResourceWarning` messages for unclosed hook stderr
readers during teardown. They did not alter the exit status and no matrix
assertion failed. This warning is recorded rather than hidden; cleanup of that
test-harness resource is not part of the four requested implementation tasks.

The workspace test log reported these passing groups: 7, 5, 9, 11, 7, 1, 7,
5, 13, 13, and 50 tests, plus zero-test targets, totaling 128 passed and zero
failed. The matrix capability assertions read each `capabilities/*.json` and
compared the published `SessionStarted.payload.capability_record` object to
the corresponding file.

## 4. Architecture changes / deviations

1. Shell scripts remain request producers; `li-hook` owns event envelope,
   sequence assignment, evidence status, queueing, and inherited sink transport.
2. Bash uses `PROMPT_COMMAND` to run builtin `history 1` into a scratch file,
   then reads it with the builtin `read`. This preserves the required history
   source without a per-command command substitution or external `rm`.
3. Bash PS0 only captures `_LI_T0`; it is not an event source. When the start
   event is enabled, the recorded start is emitted from the finish-side hook
   using the tick captured by PS0, preserving the no-fork constraint.
4. Zsh computes microseconds after taking only the first six fractional digits
   of its measured ten-digit `EPOCHREALTIME` fraction. It stores the result in
   a shell variable instead of using command substitution.
5. SessionStarted is always published by the interactive adapters once the
   inherited sink probe succeeds. Its capability record is the corresponding
   machine-readable JSON object. No capability was inferred from another
   shell.

## 5. Known problems

1. Fish dead-sink SIGPIPE behavior is **not tested**. The exact capability value
   remains `unknown:fish_is_not_killed_by_SIGPIPE_in_this_setup_untested`.
2. The capability records retain Phase 0 limitations: wall-clock shell timing,
   incomplete background completion, and `internal_script_commands: unknown`
   for this batch. P1-20 separately owns the `internal_script_commands: false`
   script-lifecycle record.
3. The matrix teardown leaves Python `ResourceWarning` messages for hook stderr
   readers. The matrix still exited 0 with all 39 combinations passing.
4. No production launcher was added here; an external launcher must provide the
   inherited FD required by C10/P1-08.
5. The pre-existing uncommitted `crates/li-hook` and workspace changes remain
   part of the working tree. This report does not claim ownership of unrelated
   preceding changes.

## 6. Security risks

| risk | status | basis |
|---|---|---|
| dead sink kills Bash/Zsh | mitigated for the tested adapters | sink writes are guarded; failures permanently disable emission; the 39/39 PTY matrix completed |
| dead sink kills Fish | open / unknown | mandated fish capability value remains the exact untested unknown string |
| shell opens an AF_UNIX socket | mitigated | adapters write only inherited FD 3; socket creation belongs to the launcher |
| shell hook stalls on transport | mitigated at client layer | inherited sink is handled by existing `li-hook` bounded send timeout and permanent silence on failure |
| capability overclaim | mitigated | records are copied field-by-field from 0-A and matrix compares SessionStarted payloads to the JSON files |
| P1-20 script lifecycle assumed complete | out of scope | no P1-20 files or behavior were added |

## 7. Next phase plan

1. Finish the launcher/daemon side needed to supply and consume the inherited
   FD, because adapters alone cannot establish the production event path.
2. Implement P1-20 lifecycle observation and keep its capability declaration
   separate from shell hook capabilities, as required by C8.
3. Only after those dependencies exist, measure the daemon restart gap and
   broader Phase 1 A2-A9 gates. P1-22 onward remains after those prerequisites.

## 8. Git status

Branch: `master`. HEAD: `77d8a4f`. No commit was made.

The working tree is **未提交** and contains the prior uncommitted `li-hook`
changes plus the new untracked `shell/` and `capabilities/` batch files and
this report. No `lumi-next` or other repository was modified.
