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
