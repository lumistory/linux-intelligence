# 0-A — Interactive shell hook capability (bash / zsh / fish)

Status: README written before any measurement (protocol §0.2).

## Objective

Decide, from measurement only, whether an interactive shell can be turned into a
reliable event source for `CommandStarted` / `CommandFinished` without a patched
shell and without a privileged daemon. Concretely:

1. For bash 5.3.20, zsh 5.9.2 (local unpacked copy, not installed), fish 4.9.3,
   find the *hook combination* that yields, per command: command text, start
   time, end time, exit status, cwd, and multi-line/pipeline/background
   fidelity.
2. Measure the added interactive latency of the hook, including the
   "daemon is dead" case, and prove that a hook whose sink is gone cannot slow
   the shell down.
3. Prove the hook does *not* fire in non-interactive shells and scripts.
4. Produce a minimal, `source`-able adapter per shell that emits one-line JSON
   on FD 3 and never blocks the shell, plus a machine-readable capability
   record per shell.

## Hypothesis

H-A1 (bash): `PS0` is useless as a command-identity source because it expands
before the command runs and sees the *previous* `$?` and `$BASH_COMMAND`; a
`PROMPT_COMMAND` + `history 1` probe does give command text and real exit
status. Therefore bash must build `CommandFinished` from `PROMPT_COMMAND`.

H-A2 (bash): the classic `DEBUG` trap must be **rejected**: it fires per simple
command (so pipelines/chains/loops produce many events where one command line
existed), it fires inside functions and subshells, and it interacts badly with
`xtrace`/`set -x`. Expected verdict: rejected. This is a *negative* hypothesis
we intend to demonstrate, not assume.

H-A3 (zsh): `preexec` + `precmd` via `add-zsh-hook` is the only combination that
delivers true start *and* end time plus exit status, because `preexec` runs
before execution and `precmd` runs before the next prompt with `$?`.

H-A4 (fish): `fish_preexec` / `fish_postexec` deliver command text in `$argv` and
status via `$status`, plus a free `$CMD_DURATION`; fish fires them for functions
too, which may or may not be desirable.

H-A5 (engineering): a hook that writes to a **nonexistent** Unix socket costs
the same order of magnitude as writing to `/dev/null` (both a single failed
`open(2)`/`write(2)`), i.e. O(10–100 µs), and an empty hook costs ~0. Therefore
"daemon unavailable" cannot be blamed for a slow shell, **provided** the sink is
never a pipe with no reader (which would block after the pipe buffer fills).

H-A6: hooks do not fire in `bash -c`, `sh script.sh`, `zsh -f script`, or
`fish -c`, because prompt machinery only exists in interactive shells.

## Method

Everything runs through a real pty (python `pty`/`select`), never through a
pipe, because prompt hooks only exist when the shell is interactive and
attached to a terminal.

- `harness/drive.py` — pty driver. Forks a pty, execs the shell with isolated
  config (`bash --norc --noprofile -i`, `zsh -f -i`, `fish --no-config -i`),
  pre-loads the adapter, then replays a step script (`RUN`/`SLEEP`/`RAW`/
  `SIGKILL`/`MARK`). Terminal bytes go to `results/<case>.term.log`; hook JSON
  goes to `results/<case>.events.jsonl` through **FD 3**, so events and
  terminal noise never mix.
- `harness/latency.py` — prompt-to-prompt timing, 100 iterations per
  configuration, median + p99, for hook={none, /dev/null, dead socket}.
- `harness/noninteractive.sh` — the negative control (H-A6).
- `harness/trap-reject.sh` — bash `DEBUG` trap evidence (H-A2).
- `adapters/{bash.sh,zsh.zsh,fish.fish}` — the minimal implementations under
  test; they are the artifacts that `run.sh` sources.
- `run.sh` — replays every case and regenerates `results/`.

13 mandatory cases (t01–t13) × 3 shells, plus engineering probes (E1 latency,
E2 non-interactive, E3 DEBUG trap, E4 pipe-block).

## Non-goals

- No host rc file is modified; no `sudo`; nothing installed system-wide.
- Shell *internal* command coverage (what a command does inside) is out of
  scope; only the command line as the shell sees it.
- nushell is not installed and is not tested.

---

## Measured findings (appended after the first measurement round)

These were discovered while building the adapters. Each one has raw evidence
under `results/`; they are recorded here so the design is not re-derived.

**F1 — PS0 lags by one command.** `PS0` expands *before* the command runs, so
`$?` and `$BASH_COMMAND` inside it describe the *previous* command line.
Evidence: `results/probe-bash-ps0-vs-promptcommand.log`.
Consequence: `PROMPT_COMMAND` + `history 1` is the only bash source of the real
command text and real exit status.

**F2 — PS0 runs in a subshell, and only some assignments survive.** `$( )` and
`${v:=word}` used as a plain expansion do **not** persist to the next
`PROMPT_COMMAND`; an assignment performed *inside an arithmetic subscript*
does, and still expands to the empty string. Evidence:
`results/probe-bash-ps0-subshell.log`. Consequence: bash gets a free start tick
via `PS0='${_LI_DUMMY[$((_LI_PEND+=1,_LI_T0=${EPOCHREALTIME/./}))]}'`.

**F3 — a write to a dead pipe kills the shell.** With a pipe whose reader has
exited, `printf >&3` terminates the interactive bash with SIGPIPE (signal 13).
`trap '' PIPE` around the write, writing from a subshell, or a failing
`connect()` all survive. Evidence: `results/probe-bash-sigpipe.log`.
Consequence: a hook must never be able to take the user's shell down with the
daemon. This is the single most important engineering constraint found.

**F4 — `trap '' PIPE` leaks into children.** After installing it, `SigIgn`
becomes `0x1000` for the shell *and* for every child and grandchild. Evidence:
`results/probe-bash-trap-pipe-inherit.log`. Consequence: install it only around
the write and restore the default immediately (`trap - PIPE`).

**F5 — any `$( )` costs ~200-250 us.** Parameter expansion is ~1 us and
`builtin history 1 >/dev/null` ~3-7 us, but every command substitution forks
and costs ~200 us in bash and ~250 us in zsh. Evidence:
`results/probe-fork-cost.log`, `results/probe-fork-cost-extclock.log`.
Consequence: at most **one** fork per command in the finish hook; a start *event*
must be opt-in, not default.

**F6 — fish needs `TERM=dumb` under a bare pty.** With `TERM=xterm-256color`
fish 4.9.3 queries the terminal (DA1, OSC11, keyboard protocol) at startup and
never paints a prompt when nobody answers, which stalls any pty harness.
Evidence: `results/probe-fish-term.log`. This is a harness constraint, not a
hook limitation.

**F7 — no shell can open an AF_UNIX socket by redirection.** Pointing `> path`
at a listening socket fails with ENXIO ("no such device or address") in bash,
zsh and fish alike; only zsh's optional `zsh/net/socket` module can dial.
Evidence: `results/probe-unix-socket-open.log`. Consequence: the sink FD must be
inherited from the launcher; the adapter never dials anything itself.

**F8 — zsh `EPOCHREALTIME` has a 10-digit fraction.** The naive
`${EPOCHREALTIME/./}` subtraction overflows signed 64-bit and yields garbage
(a 0 ms command reported as 115 ms; `sleep 0.3` reported as 302394 ms).
A microsecond integer must be built explicitly. Evidence:
`results/probe-zsh-overflow.log`. This produced a real bug in the first adapter
draft, caught by the `sleep 0.25 -> 253 ms` check.

**F9 — the DEBUG trap is rejected.** Full reasoning and counts in
`results/probe-bash-debugtrap.log` and REPORT.md section 5.

**F10 — the harness itself had to be fixed twice.** CPython ignores SIGPIPE and
the ignore flag survives `exec`, so every pty child started with SIGPIPE already
ignored and the first "dead pipe" measurements were measuring the harness, not
the shell; the child now resets SIGPIPE/SIGXFSZ to `SIG_DFL`. Also, `os.pipe()`
returns close-on-exec fds, so `dup2(w, 3)` was a no-op when the write end
already *was* fd 3 and FD 3 then vanished at `exec` (symptom: `bash: 3: Bad file
descriptor`). Both fixes are in `harness/drive.py` and `harness/probes.py`.

**F11 — a false lead, recorded because it was believed for a while.**
Adapter-latency runs kept reporting "no prompt after hook install", which looked
like the adapters' `tty` call blocking on a non-terminal FD 3. Measured
directly, `tty` does **not** block with a pipe on FD 3, for bash or for fish
(`results/probe-tty-blocks-with-pipe-fd3.log`). The real cause was F13 (an fd
ordering bug in the harness). The adapters still use `tty -s` because a status
query is strictly safer than resolving a path we only use for a log field, but
the original claim was wrong and the evidence above is what it is.

**F12 — the payload must never be spliced into the hook's command text.** The
JSON payload contains `|` (it quotes a pipeline as an example command). When the
first version of the latency harness embedded it directly, a lost quote turned
`PROMPT_COMMAND` into a real pipeline and measured 1.5 ms of shell parsing
instead of ~250 us of printf. All harnesses now assign the payload to a shell
variable first and the hook only refers to `"$__li_p"`. This is also why the
adapters pass JSON through `printf` with the payload as an *argument*.

**F13 — `os.pipe()` hands out fd 3, and dup2 over it silently breaks the child.**
In a fresh process the lowest free fds are 3 and 4, so `ev_r` was often already
3; `dup2(ev_w, 3)` then clobbered the read end and the following `close`
failed with EBADF, aborting the child (exit 111). Symptom: every FD-3 based
measurement reported "no prompt". Both `harness/drive.py` and
`harness/latency.py` now move the read end aside before claiming fd 3.

**F14 — a second false lead, corrected with evidence.** When the zsh adapter
produced no events, the cause looked like `add-zsh-hook` replacing an
already-defined function. Measured with a correct fpath, it does not: the
function still runs (`results/probe-zsh-addzshhook.log`). The real cause was that
`add-zsh-hook` could not be autoloaded at all -- this host has no system zsh, so
fpath points nowhere and the failure is `function definition file not found`.
The adapter keeps appending to `precmd_functions`/`preexec_functions` directly
because that works regardless of fpath and allows hand-written de-duplication
for idempotent re-sourcing. Two wrong diagnoses in a row is the lesson: a
"no events" symptom had at least three distinct causes here (fpath, module_path,
and harness fd misuse).

**F15 — the two shells disagree on EPOCHREALTIME precision.** bash 5.3 reports a
6-digit fraction (microseconds); this zsh build reports 10 digits (nanoseconds).
A zsh adapter that copies the bash arithmetic either overflows 64-bit or emits
`ts_wall` values like `...:29.3777232170Z`, which is not ISO-8601. The zsh
adapter truncates the fraction to 6 digits before both arithmetic and formatting.
Evidence: `results/probe-clock-precision.log`.

**F16 — the hook body itself is cheap; the prompt is not.** Measured from inside
`/bin/sh` with the shell's own clock (`results/latency/hookcost.json`):

| shell | hook body, median | notes |
|---|---|---|
| bash | **3 µs** | one `history 1` fork included |
| zsh | **2348 µs** | dominated by `$( )` for the start clock and event line |
| fish | **1085 µs** | batch loop, `date` cost amortised |

The bash adapter is therefore effectively free. The zsh and fish adapters cost
about 1-2 ms per command, which is real and visible to a user typing fast.
