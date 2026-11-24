# 0-B — Non-interactive script lifecycle: report

Run of record: `results/20261123T133700Z/` (driver log:
`results/20261123T133700Z/driver.log`). 10 cases, **41 assertions passed,
0 failed**, `run.sh` exited 0.

Reproducibility: the same driver was run again as
`results/20261123T134924Z/` — again **41 assertions passed, 0 failed**. Case
verdicts and process counts reproduced exactly. Wall-clock durations and the
per-scan cost did not: that later run measured ~4.75 ms per `/proc` scan instead
of ~2.4 ms, because the machine was carrying 659 processes instead of 522. All
numbers quoted below come from the run of record. This is itself a finding: the
sampling floor is load-dependent, so §4.5 must be read as "on this host, at this
load".

## 1. Objective

Determine what a supervisor process can observe about a script it launches with
fork/exec — with **no shell cooperation whatsoever** — and draw the exact line
between what is trustworthy and what is not.

Concretely: `li-run-proto -- <program> <args...>` starts the target in its own
session (`setsid`), records identity and timing, reconstructs the descendant
process set from `/proc`, and emits `ScriptStarted` / `ScriptFinished` JSON
events in the shared envelope.

## 2. Hypothesis

| id | hypothesis | verdict |
|---|---|---|
| H-B1 | process-level facts (pid, argv, cwd, exit status, signal, duration) are `observed` and trustworthy | **PASS** |
| H-B2 | internal commands of the script are **not** observable; output exists only as an unattributed byte stream | **PASS** (quantified below) |
| H-B3 | a pre/post `/proc` snapshot diff misses short-lived processes and mis-attributes orphans, so continuous sampling + session-id lineage is required | **PASS** |

## 3. Method

- Rust crate `li-run-proto` (deps `nix`, `libc`, `serde_json`, `thiserror`;
  built `--offline`). `cargo test`: 7 unit tests pass. `cargo clippy
  -- -D warnings`: clean. `cargo fmt --check`: clean.
- Child spawned via `Command` with `pre_exec { libc::setsid() }`, stdin
  `/dev/null`, stdout/stderr redirected to files so the JSON event stream on
  the supervisor's stdout stays parseable.
- `libc::wait4` used instead of `Child::wait`, to get the child's `rusage` in
  the same syscall. This is one of the two `unsafe` blocks in the crate
  (§6 justifies them).
- Descendants: a sampler thread reads the whole `/proc` table every
  `--poll-us` (default 2000) and keeps every process in the child's **session**,
  plus the parent-closure over those (catching a descendant that calls
  `setsid` itself). A pre/post full-table snapshot diff is computed **in
  parallel, and reported separately**, so the two methods can be compared.
- Signals are delivered with `killpg(child_pgid, SIG)` — identical to what the
  kernel does for `^C` on a controlling terminal. The harness has no controlling
  terminal, so a genuine tty `^C` is covered by experiment 0-D instead.
- All scripts are in `scripts/`, all raw output in `results/<ts>/<case>/`, all
  claims asserted by `run.sh` with `jq`.

## 4. Raw results

### 4.1 Case summary

Source: `results/20261123T133700Z/summary.tsv`

| case | exit code | signal | duration ms | procs recorded | survivors |
|---|---|---|---|---|---|
| single | 0 | — | 0.79 | 1 | 0 |
| nested (script→script→script) | 0 | — | 206.68 | 5 | 0 |
| background (`sleep &`) | 0 | — | 1.13 | 2 | **1** |
| execrepl (`exec sleep`) | 0 | — | 502.87 | 2 | 0 |
| execrepl-fast (negative control) | 0 | — | 1.21 | 1 | 0 |
| exit3 | **3** | — | 0.84 | 1 | 0 |
| ctrlc (SIGINT to pgroup) | — | **SIGINT (2)** | 400.43 | 2 | 0 |
| kill9 (SIGKILL to pgroup) | — | **SIGKILL (9)** | 400.34 | 2 | 0 |
| bigoutput | 0 | — | 964.97 | 1 | 0 |
| boundary | 0 | — | 4.01 | 1 | 0 |

`ScriptStarted` for `single` (`results/.../single/events.jsonl`):

```json
{"program":"/bin/bash",
 "argv":[".../scripts/single.sh"],
 "cwd_at_start":"/tmp/li-phase0-b-20261123T133700Z",
 "pid":101435,"sid":101435,
 "capabilities":{"builtins":false,"descendant_processes":"partial:sampled",
   "duration":true,"exit_status":true,"internal_script_commands":false,
   "output_attribution":false,"process_identity":true,"redirections":false}}
```

### 4.2 Descendant reconstruction, `nested` (3 script levels)

`results/20261123T133700Z/nested/events.jsonl` → 5 processes, real ppid chain:

| pid | ppid | comm | samples |
|---|---|---|---|
| 101500 | 101498 | bash (leader) | 46 |
| 101503 | 101500 | bash (child.sh) | 45 |
| 101504 | 101503 | sleep | 22 |
| 101506 | 101503 | bash (grandchild.sh) | 22 |
| 101507 | 101506 | sleep | 22 |

### 4.3 Orphan survival, `background`

`results/20261123T133700Z/background/events.jsonl`:

| pid | ppid | comm | alive after leader exit | source |
|---|---|---|---|---|
| 101521 | 101519 | bash | false | leader_record |
| 101523 | **940** | sleep | **true** | sampled |

The `sleep` was reparented to pid 940 (`systemd --user` on this host, not pid 1
— see §6) and was still running at the final sweep. `survivors_after_exit` is
non-empty, proving the supervisor can detect "the script left a process behind".
It cannot, however, do anything about it without an explicit policy; `run.sh`
does not kill it, and the test asserts only that it was *seen*.

### 4.4 `exec` replacement

`execrepl` (dwells 200 ms, then `exec /usr/bin/sleep`):

| pid | exe_first | exe_last | samples |
|---|---|---|---|
| 101541 | /usr/bin/bash | **/usr/bin/sleep** | 107 |
| 101543 | /usr/bin/sleep | /usr/bin/sleep | 41 |

Same pid, image changed, `pid == .payload.pid` → the leader's identity survived
`exec`. This is the strongest single argument for pid-based lineage.

`execrepl-fast` (negative control, `exec ls` immediately, total lifetime
~1.2 ms): the leader was captured at spawn as `bash` and **never sampled**, so
`exe_last` stayed `/usr/bin/bash` and the `bash → ls` transition was **not
observed**. Assertion `execrepl-fast.no_image_change == 0` passes — i.e. the
test records that the mechanism genuinely fails at this timescale.

### 4.5 Detection probability vs. process lifetime

Source: `results/20261123T133700Z/detection-rate.tsv`. 50 identical external
commands per row (`scripts/lifetime.sh <repeat> <dwell>`), counting distinct
recorded processes with `comm == sleep`.

| dwell ms | 200 µs period | 2000 µs period | 10000 µs period |
|---|---|---|---|
| 1 | 0/50 (0.00) | 0/50 (0.00) | 0/50 (0.00) |
| 5 | 50/50 (1.00) | 27/50 (0.54) | 13/50 (0.26) |
| 20 | 50/50 | 50/50 | 50/50 |
| 100 | 50/50 | 50/50 | 50/50 |

Measured cost of one full `/proc` scan on this host (522 processes, 20 cores,
7.2.8-zen), from `sampling.mean_read_ns` and `scan-cost-env.txt`:

| measurement | value |
|---|---|
| sampler's own mean scan | **2.37 ms** (nested run, 103 reads) |
| sampler's max scan | 5.39 ms |
| naive `ls -d /proc/[0-9]*`, mean of 10 | **2.79 ms** |
| processes in table | 522 |

**A scan costs ~2.4 ms, so the achievable sampling period is ~2.4 ms, not the
200 µs requested.** The requested 200 µs rows above are therefore effectively
~2.4 ms rows; that is why 1 ms processes score 0/50 even "at 200 µs".

### 4.6 The observability boundary, quantified

`scripts/boundary.sh` does: 6 `echo` lines, 7 silent builtins
(`:`/assignment/`cd`/`cd -`/`umask`/`unset`), 1 redirect to a file outside
stdout, and 3 short-lived external commands (`/bin/true`, `/bin/false`,
`/usr/bin/printenv`).

| what the script did | count | what the supervisor saw |
|---|---|---|
| `echo` lines | 6 | **6 lines of text**, in one aggregate stream, unattributed |
| silent builtins | 7 | **0** — no process, no fork, nothing |
| `> file` redirect | 1 | **0 events**; the bytes never enter the supervisor's stream |
| external commands (`true`/`false`/`printenv`) | 3 | **0** — all ~1 ms, below the ~2.4 ms scan |
| `CommandStarted`/`CommandFinished` events | 0 | **0** — the prototype never emits any |
| processes recorded | 1 | the leader only (`observation_source: leader_record`) |

The full 6-line text the supervisor received
(`results/.../boundary-stdout.txt`):

```
BOUNDARY-ECHO-1 .. BOUNDARY-ECHO-6
```

and the redirected line it never saw at all
(`results/.../boundary-redirected.txt`, written to `/tmp/li-phase0-b-*`):

```
BOUNDARY-REDIRECTED
```

So the answer to "5 echoes: how many can you see?" is: **you see all of the
text and none of the structure.** With 5 or 6 `echo` lines you get 5 or 6 lines
of output, but you cannot say which line belongs to which command, cannot tell
`echo` from `printf` from a heredoc, cannot see a builtin at all, and cannot see
redirections. The text count is not evidence about commands.

### 4.7 Output volume vs. event volume

`bigoutput` (2000 × `echo` + 2000 × `/bin/echo`):

| metric | value |
|---|---|
| stdout lines | **4000** |
| stdout bytes | 35 786 |
| processes recorded | **1** (the leader) |
| events emitted | **2** (`ScriptStarted`, `ScriptFinished`) |

4000 output lines, 4000 command invocations, and the supervisor's event stream
grew by exactly two records. This is the cleanest illustration that stdout
carries *effects*, not *actions*.

## 5. Analysis

### 5.1 What is trustworthy: `Script Lifecycle Observed`

These are first-hand facts of the supervisor or kernel reads, `evidence_status =
observed`, and I would put them in a database without qualification:

- `program` and `argv` — we passed them to `exec`.
- `cwd` at start — our own cwd, plus `/proc/<pid>/cwd` cross-check.
- `pid`, `pgid`, `sid` — we forked it and it called `setsid`, so all three are
  equal; verified for every case (`single.pid_is_sid_and_pgid` passed).
- start/end time on **both** `CLOCK_MONOTONIC` (`start_mono_ns`,
  `spawn_mono_ns`, `exit_mono_ns`) and wall clock, plus `duration_ns`.
  Monotonic ordering was asserted: `start < spawn < exit`.
- exit code **or** terminating signal, mutually exclusive, from `wait4`.
  `exit3` → 3; `ctrlc` → `SIGINT (2)` with `exit_code: null`; `kill9` →
  `SIGKILL (9)`. The supervisor's own exit status mirrors these as
  `0/3/130/137`, which the driver asserts independently.
- `rusage` (user/sys time, maxrss, nvcsw/nivcsw) of the reaped child.
- **existence and identity** of descendants: pid, ppid, sid, comm, exe, cmdline,
  first/last-seen timestamps, observed lifetime.
- the fact that *something* survived the script (`survivors_after_exit`).
- total bytes/lines written to stdout and stderr.

### 5.2 What is not obtainable: `Script Internal Commands Observed`

This experiment makes **no claim** about internal commands, and the measurements
in §4.5–4.7 show why it cannot:

1. **Builtins are invisible.** 7 builtins produced zero processes. There is no
   kernel object to observe. Only a shell hook (0-A) can see them.
2. **Short commands are a lottery.** At ~1 ms lifetime, 0/50 were seen at every
   sampling rate; at 5 ms, 54 % at a 2 ms period; at 20 ms, 100 %. Since a scan
   costs ~2.4 ms, the ceiling is a ~2.4 ms window and the detection probability
   is roughly `min(1, lifetime / 2.4 ms)` — models roughly, but the 5 ms row
   (54 %, not 100 %) shows it is a coin flip in the transition band.
3. **No attribution.** The 6 echoes in §4.6 arrive as one stream with no
   boundaries. Even the 4000-line `bigoutput` case yields 2 events.
4. **Redirections vanish** from the supervisor's view entirely.
5. **Sampling can also over-report**: a background `sleep` recorded after the
   script ended was reparented to pid 940 (`systemd --user`, a subreaper-like
   helper on this host) rather than pid 1, so "orphan" detection needs the
   *recorded* ppid, not an assumption that orphans go to pid 1.

The honest capability record is therefore
`"internal_script_commands": false`, `"builtins": false`,
`"redirections": false`, `"output_attribution": false`, and
`"descendant_processes": "partial:sampled"` — exactly what `ScriptStarted`
carries in §4.1.

### 5.3 Snapshot diff vs. continuous sampling

Both are computed and reported (`payload.snapshot_diff` vs
`payload.processes`). The pre/post diff is pid-keyed against the whole table and
so is structurally incapable of seeing anything that exits during the run — it
returned the orphan only because that orphan was still alive at the end. Every
short-lived process in `nested`, `bigoutput` and `boundary` was found by the
sampler, not by the diff. Conclusion: the diff is useful only as a cheap
sanity check; **sampling plus session-id lineage is the load-bearing mechanism.**

### 5.4 A defect this experiment found in itself

The first version of the prototype recorded `single` with `process_count: 0`:
the script lived 0.79 ms, shorter than one scan, so the sampler never saw even
the leader — and re-reading `/proc/<pid>` *after* `wait4` also failed, because
the zombie had already been reaped. Fix: capture the leader's `/proc` record
immediately after spawn and keep it, tagged
`observation_source: "leader_record"` so the distinction from `sampled` is
preserved in the data. Recorded verbatim because it is a real trap: any
"reconstruct after the fact" design is broken by construction.

## 6. Known limitations

**NOT TESTED / not covered:**

1. **NOT TESTED: a real tty `^C`.** This harness has no controlling terminal
   (`/proc/self/fd/0 -> /dev/null`), so `^C` was emulated with
   `killpg(child_pgid, SIGINT)`. That is the same signal the line discipline
   would generate, but the path *through* a terminal is not exercised here — it
   is exercised in experiment 0-D.
2. **NOT TESTED: cgroups / pidfd / eBPF based tracking.** These would close the
   short-lived-process gap. Deliberately out of scope; see §8.
3. **NOT TESTED: cross-distro behaviour**, non-Linux, or a different `/proc`
   layout (0-F).
4. **NOT TESTED: scripts that daemonize** (double fork + `setsid`). The
   parent-closure logic is written for it but no test exercises it.
5. **NOT TESTED: more than ~1 script at a time.** Concurrency of the *supervisor*
   is untested; 0-D covers PTY concurrency, not script concurrency.
6. **NOT TESTED: `argv` with NUL bytes / non-UTF-8.** `CString::new` failure
   paths exist but are unexercised.
7. **Sampling cost is host-dependent.** The ~2.4 ms scan on 522 processes will be
   worse on a host with 5000 processes; the detection table does not hold there.
8. **Orphan reparenting is host-specific.** Orphans went to pid 940
   (`systemd --user`), not pid 1. The prototype never assumes pid 1, but a
   consumer must not either.
9. **Timing precision.** `duration_ms` is derived from `CLOCK_MONOTONIC` around
   `spawn`/`wait4`, so it includes fork+exec+reap overhead and is not
   "the program's runtime"; it is the *observed* runtime.

**`unsafe` justification** (2 blocks, both unavoidable):

- `cmd.pre_exec(|| libc::setsid())` — `setsid` between fork and exec cannot be
  done from safe Rust; it is async-signal-safe.
- `libc::wait4` — the standard library's `Child::wait` discards `rusage`.

Both are annotated with `// SAFETY:` comments. `panic = "abort"` in release.

**Deviations from protocol:** none. `run.sh` writes only inside this experiment
directory and `/tmp/li-phase0-b-*`; no `sudo`, no `rm -rf`, no rc-file edits.

## 7. Conclusion

**PASS-WITH-LIMITS.**

H-B1 passes outright: every process-lifecycle fact is `observed`, cross-checked
against `wait4`, and asserted 41 times.

H-B2 passes and is now quantified rather than asserted: 6 echoes yield 6 lines
of unattributed text, 7 builtins yield nothing, 3 external commands at ~1 ms
yield nothing, and 4000 command invocations yield exactly 2 events. The
boundary is not a tuning problem — it is what a non-shell supervisor can know.

H-B3 passes: continuous sampling with session-id lineage is the only mechanism
that found the nested tree (5 processes over 3 levels) and the surviving orphan;
the snapshot diff alone could not have.

Limits are those in §6, chiefly the ~2.4 ms sampling floor, untested tty `^C`,
and untested daemonizing scripts.

## 8. Gate

**May Phase 1 implement this? YES, with conditions.**

Adopt, as-is:

1. `setsid` + `pid/pgid/sid`-equal lineage as the identity backbone. `exec`
   survival (§4.4) and 3-level nesting (§4.2) make this non-negotiable.
2. `wait4` for exit status + `rusage`, and dual monotonic/wall timestamps.
3. Leader-record-at-spawn (§5.4). Without it, every sub-3 ms script reports zero
   processes.
4. Emitting `capabilities` with `internal_script_commands: false` on every
   `ScriptStarted`, so no downstream consumer can mistake process data for
   command data.

Conditions before Phase 1 relies on it:

5. **Do not ship `/proc` sampling as the descendant mechanism alone.** §4.5
   shows 0/50 at 1 ms. If per-command fidelity is a Phase 1 requirement, it needs
   a cgroup v2 (`cgroup.procs` + `cgroup.events`) or eBPF (`sched_process_*`)
   provider — that is a new experiment, not a tuning exercise.
6. **Specify the two-tier claim explicitly in the product schema.** Tier 1
   (lifecycle, trustworthy) and Tier 2 (internal commands, unavailable without a
   shell hook). Mixing them in one event type would be the single most damaging
   design mistake available here.
7. **Test the tty `^C` path** (0-D does) and **daemonizing scripts** before
   claiming descendant completeness.

> **路径替换说明**：本次公开前对 `results/` 与脚本里的绝对路径做了字符串替换，换成两个占位符 `<WORKSPACE>` 与 `<HOME>`。
> 规则、未改动的量、以及原始形态见 [`../SCRUB-NOTICE.md`](../SCRUB-NOTICE.md)。
