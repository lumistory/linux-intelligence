# 0-B — Non-interactive script lifecycle

Status: README written before any measurement (protocol §0.2).

## Objective

Determine exactly what a supervisor process can observe about a script it
launches with fork/exec (`li run -- <program> <args...>`), when the supervisor
is **not** a shell and therefore gets no shell hooks at all.

The prototype binary `li-run-proto` must:

1. start the target program in its own session / process group (`setsid`);
2. record program, argv, cwd at start, pid/pgid/sid, start and end time on both
   `CLOCK_MONOTONIC` and wall clock, exit code **or** terminating signal, and
   duration;
3. after the script exits, reconstruct the descendant process set from
   `/proc` (pre/post parent-tree snapshot diff, plus continuous sampling);
4. emit one JSON event per line on stdout (`ScriptStarted`, `ScriptFinished`)
   using the shared event envelope of `docs/00-phase0-protocol.md`, with the
   collected process list attached to `ScriptFinished`.

## Hypothesis

H-B1: process-level facts (pid, argv, cwd, exit status, signal, duration) are
`observed` — the supervisor gets them from `wait4` and its own clock, so they
are trustworthy.

H-B2: **internal commands of the script are not observable.** The supervisor
sees one process and an aggregate stdout/stderr byte stream. Concretely, for a
script containing five `echo` lines, the supervisor sees five lines of text
but cannot attribute any of them to a command, cannot see builtins or
redirections at all, and sees even external commands only if the ~ms lifetime
of the process happens to fall inside a `/proc` sampling window.

H-B3: pre/post `/proc` snapshot diff alone misses everything that starts and
exits while the script runs, and mis-attributes orphans (reparented to PID 1)
— so it must be complemented by continuous sampling and by session-id
membership, which `setsid` makes a reliable lineage marker.

## Method

- Rust prototype `li-run-proto` (crate in this directory, `--offline`, deps:
  `nix`, `libc`, `serde`, `serde_json`, `thiserror`).
- Child is spawned with `Command` + `pre_exec { setsid() }`, stdin `/dev/null`,
  stdout/stderr redirected to files in `--out-dir` (so the JSON event stream on
  the supervisor's stdout stays parseable).
- `pre_exec` uses `libc::setsid()` (async-signal-safe).
- `libc::wait4` is used instead of `Child::wait` to obtain per-child `rusage`;
  this is the single `unsafe` block in the crate and is justified in REPORT §5.
- Descendants are found by a sampler thread that reads the whole `/proc` table
  every `--poll-us` microseconds (default 3000) and keeps every process whose
  session id equals the child's session id, plus its parent chain, for the
  whole lifetime of the script. A pre/post full-table snapshot diff is also
  computed and reported separately, so the two methods can be compared.
- Signals are delivered with `killpg(child_pgid, SIG)` — the same thing the
  kernel does for `^C` on a controlling terminal. There is no controlling
  terminal in this harness (the harness's own stdin is `/dev/null`), so a
  genuine tty `^C` is covered by experiment 0-D instead.

### Script samples (must all be run)

| case | script | what it is for |
|---|---|---|
| `single` | one command | baseline |
| `nested` | script calls script calls script | multi-level ppid tree |
| `background` | `sleep N &` then exit | orphan survives the script |
| `execrepl` | `exec ls` | same pid, different image |
| `ctrlc` | `sleep 30`, SIGINT to pgroup | 130 / Signaled |
| `exit3` | `exit 3` | normal non-zero exit |
| `kill9` | `sleep 30`, SIGKILL to pgroup | Signaled(9) |
| `bigoutput` | ~4000 lines | output volume vs. event volume |
| `boundary` | 5×`echo` + silent builtins + redirect + 1 child | the H-B2 boundary, quantified |

`run.sh` runs all cases, writes raw logs to `results/<UTC timestamp>/`, and
asserts the recorded facts with `jq`. It exits non-zero if any assertion fails.

## Out of scope

- shell hooks / `PROMPT_COMMAND` / DEBUG traps: that is experiment 0-A. This
  experiment deliberately has no shell cooperation at all.
- cgroups / pidfd / eBPF based tracking: mentioned in REPORT §8 as the way to
  close the gaps found here, but not implemented.
- Cross-distro behaviour: 0-F.
