# 0-A — Interactive shell hook capability: bash / zsh / fish

Run of record: `results/analysis.json` (13 cases × 3 shells = **39/39 executed**),
per-case transcripts `results/t01..t13.<shell>.{term.log,events.jsonl}`,
capability records `capabilities/{bash,zsh,fish}.json`, latency
`results/latency/latency.json` (180 samples per mode). bash 5.3.20 (system),
zsh 5.9.2 (unpacked local copy, **not installed system-wide**), fish 4.9.3.

Every number below is copied from a file under `results/`. Anything not measured
is `NOT TESTED` with the reason. `partial:` means "works, with a named
limitation", and the limitation is written down.

## 1. Objective

Decide from measurement whether an unmodified interactive shell can be a
reliable event source for `CommandStarted` / `CommandFinished`, and exactly
where each shell stops being reliable. Concretely:

1. Find the hook combination per shell that yields command text, start time, end
   time, exit status and cwd.
2. Measure the latency the hook adds, including when the sink (daemon) is gone.
3. Prove the hook is silent in non-interactive shells.
4. Prove a dead sink cannot kill or hang the shell.
5. Emit a machine-readable capability record per shell.

## 2. Hypothesis

| # | Hypothesis | Verdict |
|---|---|---|
| H-A1 | bash `PS0` cannot be the command source: it expands *before* the command and sees the previous `$?` and previous `$BASH_COMMAND`. `PROMPT_COMMAND` + `history 1` can. | **PASS** |
| H-A2 | The classic `DEBUG` trap must be rejected: it fires per *simple* command, so pipelines/chains/loops over-report, and it interleaves with `set -x`. | **PASS (rejected on evidence)** |
| H-A3 | zsh `preexec` + `precmd` via `add-zsh-hook` is the only pair giving both real start and end plus exit status. | **PASS** |
| H-A4 | fish `fish_preexec` / `fish_postexec` deliver text and status, with `fish_postexec` authoritative. | **PASS** |
| H-A5 | A hook can be made unable to block the shell. | **PASS in the hang sense, FAIL in the cost sense** — it cannot *block*, but a dead-socket attempt costs ~1.2 ms (bash), which is a real per-command tax |
| H-A6 | A raw write to a dead sink is survivable. | **FALSIFIED** — a raw write to a closed pipe **kills the interactive shell** via SIGPIPE |

## 3. Method

| file | role |
|---|---|
| `harness/drive.py` | PTY driver: spawns a shell, feeds a case script, captures the terminal stream and the adapter's FD-3 stream |
| `harness/cases.py` | the 13 cases as data (t01–t13) |
| `harness/probes.py` | one-off probes: PS0 vs PROMPT_COMMAND, DEBUG trap, SIGPIPE, fork cost, non-interactive silence |
| `harness/li_emit.c` | tiny C emitter (startup cost matters; see t13 and §4.5) |
| `harness/latency.py` | prompt-to-prompt latency harness, 4 sink modes × 3 shells × 180 samples |
| `adapters/bash.sh` | `PROMPT_COMMAND` (finish: text + status) + zero-width `PS0` (start tick) |
| `adapters/zsh.zsh` | `preexec` / `precmd` via `add-zsh-hook` |
| `adapters/fish.fish` | `fish_preexec` / `fish_postexec` |
| `capabilities/*.json` | the machine-readable capability records, including `known_gaps` and `rejected_alternatives` |

The adapters emit one JSON object per line on **FD 3**, which the launcher
supplies. They never open a socket themselves; see §4.5 for why.

## 4. Raw results

### 4.1 Coverage — 13 cases × 3 shells

`results/analysis.json`, event counts per case (`n_events` includes the shell's
own `exit` line where the shell reports it):

| case | what it tests | bash | zsh | fish |
|---|---|---|---|---|
| t01 | plain command | 1/1 | 1/1 | 3/2 |
| t02 | backslash continuation | 2/2 | 2/2 | 4/3 |
| t03 | pipeline | 1/1 | 1/1 | 3/2 |
| t04 | `&&`, `\|\|`, `;` | 4/4 | 4/4 | 6/5 |
| t05 | builtins (`cd`, `export`/`set -gx`) | 4/4 | 4/4 | 5/4 |
| t06 | function definition + call | 2/2 | 2/2 | 4/3 |
| t07 | alias define + invoke | 2/2 | 2/2 | 4/3 |
| t08 | background `&` + `wait` | 3/3 | 3/3 | 5/4 |
| t09 | subshell + command substitution | 2/2 | 2/2 | 4/3 |
| t10 | `^C` | 3/3 | 3/3 | 5/4 |
| t11 | shell killed with `kill -9` | 1/1 | 1/1 | 2/1 |
| t12 | large output + redirection | 3/3 | 3/3 | 5/4 |
| t13 | 10 rapid `echo`s | **10/10** | **10/10** | 12/11 |

`bash`/`zsh` columns read `events/user-commands`. fish reports its own `exit`
line as a command, so it is one ahead; that is not a defect, it is a capability
difference (see `capabilities/fish.json`).

**The important rows:** t13 shows ten back-to-back commands produce ten events
in order, no loss, in all three shells. t10 shows `^C` yields exit status
**130** in all three. t11 shows `kill -9` of the shell produces **no unclosed
`CommandStarted`** — the shell dies without emitting a finish, and
`unclosed_event_on_kill9: false` in the capability record means the adapter
does not emit a start it can never close.

### 4.2 bash: PS0 is not the command source (`results/probe-bash-ps0-vs-promptcommand.log`)

Measured, verbatim from the probe:

- `PS0` expands **before** the command runs, and at that moment `$?` is the
  *previous* command's status and `$BASH_COMMAND` is the *previous* command.
- `PROMPT_COMMAND` runs before the prompt with the real `$?` of the command that
  just finished.
- `HISTTIMEFORMAT='' history 1` inside `PROMPT_COMMAND` yields the full command
  line, including a backslash-continued multi-line command **joined into one
  line**.

Consequence, and this is the design: bash builds `CommandFinished` from
`PROMPT_COMMAND` + `history 1`, and uses `PS0` only for a zero-width start tick.
`command_text_multiline` is therefore
`partial:joined_with_single_space_backslash_removed` — the text is right, the
newline is gone.

### 4.3 DEBUG trap rejected on evidence (`results/probe-bash-debugtrap.log`)

The trap fires per **simple** command. For a single typed pipeline or `&&`
chain it fires several times; inside a function it fires for the function body;
inside a subshell it fires again. The count of trap firings does not match the
count of typed command lines, so any event stream built on it over-reports and
cannot be reconciled with what the user typed. Recorded in
`capabilities/bash.json → rejected_alternatives`.

### 4.4 A dead sink can kill the shell (`results/probe-bash-sigpipe.log`)

```
raw write to dead pipe             shell_survived=False SIGPIPE_died=True
write inside trap '' PIPE          shell_survived=True  SIGPIPE_died=False
write from a subshell              shell_survived=True  SIGPIPE_died=False
python connect to dead socket      shell_survived=True  SIGPIPE_died=False
```

A raw write to a closed pipe **kills the interactive shell**. `trap '' PIPE`
around the write survives — but the ignore flag is inherited by children
(`SigIgn` bit 13), which changes the behaviour of every program the user runs
from that shell. The chosen mitigation is therefore the third and fourth rows:
**write from a subshell, or use a `connect()` that fails cleanly**. The adapters
take the launcher-supplied FD and a subshell write.

### 4.5 Per-command cost, and why FD 3 (`results/latency/latency.json`, `results/probe-fork-cost*.log`)

180 samples per cell, prompt-to-prompt:

| shell / sink | p50 µs | p99 µs | max µs |
|---|---|---|---|
| bash / empty (baseline) | 202 | 714 | 907 |
| bash / `/dev/null` | 256 | 583 | 600 |
| bash / **dead socket** | **1168** | 1575 | 2243 |
| bash / live socket | 1160 | 1739 | 1773 |
| zsh / empty | 192 | 433 | 483 |
| zsh / `/dev/null` | 197 | 347 | 380 |
| zsh / **dead socket** | **1488** | 2039 | 2049 |
| zsh / live socket | 1456 | 1741 | 2115 |
| fish / empty | 1079 | 1991 | 2917 |
| fish / `/dev/null` | 404 | 641 | 705 |
| fish / **dead socket** | **346** | 622 | 676 |
| fish / live socket | 386 | 613 | 642 |

Two separate costs, and they must not be conflated:

1. **A `$(...)` command substitution in a bash hook costs ~190 µs** on its own
   (`results/probe-fork-cost.log`). This is why the adapter avoids command
   substitution and why `li_emit` is a compiled C helper rather than a Python or
   shell script.
2. **Trying to talk to an absent daemon costs ~1.2 ms in bash and ~1.5 ms in
   zsh** — 5–8× the cost of a trivial local write. fish is different: its
   `preexec` is cheap enough that the dead-socket attempt is only ~0.35 ms.

So the honest statement is: **the hook cannot hang the shell (0-C's write
timeout is the reason), but it does tax every command.** bash and zsh pay about
1.2–1.5 ms per command when the daemon is down. That is the number Phase 1 must
design against, and it argues for a **persistent connection attempt in the
background**, not a connect-per-command.

### 4.6 Non-interactive shells are silent (`results/noninteractive.log`)

| invocation | FD-3 bytes | verdict |
|---|---|---|
| `bash -c` (sources adapter) | 0 | SILENT |
| `bash script.sh` | 0 | SILENT |
| `bash -i -c` | 0 | SILENT |
| `zsh -f -c` | 0 | SILENT |
| `zsh -f script` | 0 | SILENT |
| `zsh -f -i -c` | 0 | SILENT |
| `fish -c` | 0 | SILENT |
| `fish script.fish` | 0 | SILENT |

All eight silent. Note the third and sixth rows: **forcing `-i` still does not
make the hook fire** under `-c`, because the adapters gate on the shell actually
having an interactive tty, not merely on `-i`.

## 5. Analysis

**Each shell needed a different mechanism, and the differences are real
capabilities, not stylistic.** bash loses the newline in a multi-line command
(it is recovered from `history 1`), fish concatenates continuations without a
separator, zsh preserves literal newlines. zsh reports `duration` from a
wall-clock delta; fish provides `CMD_DURATION` natively. Any design that assumes
one shape would have to discard information from the shell that has more of it,
or invent information for the shell that has less.

**`partial:` is the useful part of the capability record.** The record does not
say "bash supports multi-line commands"; it says
`partial:joined_with_single_space_backslash_removed`. A consumer that needs the
newline knows immediately that bash cannot give it.

**The two negative results are the most valuable output.** The DEBUG trap
over-reports in a way that silently corrupts a command count. The raw dead-pipe
write kills the shell outright. Both were plausible-looking designs; both are
now excluded with evidence rather than opinion.

**The cost result reframes the architecture.** 0-C proved a write with no
timeout can hang for seconds, and this experiment proves the *safe* version
still costs 1.2–1.5 ms per command in bash and zsh. A connect-per-command design
is therefore wrong for the wrong reason: not because it hangs, but because it
taxes every single command the user types. The right shape is a long-lived
background connection with a bounded, lossy queue — which is exactly what ADR
0002's client rules describe.

**fish's cost profile is inverted.** Its `preexec` baseline (1079 µs "empty") is
*slower* than its dead-socket case (346 µs), because the empty baseline includes
fish's own prompt work. This is a reminder that prompt-to-prompt timing measures
the whole prompt cycle, not only the hook — the ranking across shells is
meaningful, the absolute values are not comparable across shells.

## 6. Known limitations

**NOT TESTED / not covered:**

1. **NOT TESTED: zsh as installed by a distribution.** All zsh results depend on
   an isolated `ZDOTDIR` shim that sets `module_path` and `fpath` into the
   unpacked tree, because the local copy has no `/usr/lib/zsh`. A
   system-installed zsh needs no shim, and its results might differ (most likely
   in latency only).
2. **NOT TESTED: bash 4.x, zsh 5.8, fish 3.x.** Only the three specific versions
   above were measured. `PS0` exists from bash 4.4; older bash has no start
   tick at all.
3. **NOT TESTED: nushell.** Not installed. Its REPL hooks cannot report script
   internals either, so the Tier-1/Tier-2 split applies, but no measurement
   backs that statement.
4. **NOT TESTED: interactive multi-line editing (a here-document typed at the
   prompt).** Only backslash continuation was tested. A here-doc is a different
   `history` shape.
5. **NOT TESTED: a hook inside `tmux`/`screen` or over SSH.** The PTY driver
   owns the terminal; nesting was not exercised.
6. **NOT TESTED: 1 000-command sessions.** t13 covers ten rapid commands. A
   long-session sequence-gap measurement belongs to Phase 1's acceptance tests.
7. **`duration` is wall clock everywhere.** An NTP step or a suspend/resume
   skews it. No shell here exposes a monotonic clock to a hook
   (`monotonic_clock: false` in every capability record).
8. **fish reports its own `exit`** as a command, so a naive consumer will count
   one extra command per session. This is recorded, not fixed, because hiding it
   would be worse.
9. **Known gaps carried in the capability records and repeated here:** in bash
   the `source adapters/bash.sh` line itself is never reported; `exit` is never
   reported (PROMPT_COMMAND does not run at shell exit); `exec` replaces the
   shell so no finish event; a background job reports the launch line, never its
   completion.

## 7. Conclusion

**PASS-WITH-LIMITS.** 39/39 cases executed, in all three shells, with no
invented results and two hypotheses falsified on evidence.

What Phase 1 may rely on:

1. `CommandFinished` with command text, exit status and cwd — solid in all three
   shells.
2. `CommandStarted` with a start tick — real in all three, but wall-clock only.
3. Ten back-to-back commands, no loss, correct order.
4. `^C` → status 130, in all three.
5. Silence in non-interactive shells, including forced `-i -c`.

What Phase 1 must **not** claim:

- Multi-line command text with newlines preserved (bash loses it, fish mangles
  the separator).
- Background job completion (only the launch line is observable).
- Duration as a monotonic measurement.
- Any per-command cost below ~1.2 ms for bash/zsh when the daemon is absent.

## 8. Gate

**May Phase 1 implement the shell adapters on this baseline? YES, with
conditions.**

Adopt as-is:

1. **bash**: `PROMPT_COMMAND` + `history 1` is the event source; `PS0` is a
   zero-width start tick only. `DEBUG` trap is excluded.
2. **zsh**: `preexec` / `precmd` via `add-zsh-hook`, with an `fpath`/`module_path`
   fallback for non-system installs.
3. **fish**: `fish_preexec` / `fish_postexec`, `fish_postexec` authoritative,
   `CMD_DURATION` used for duration.
4. **Emit on a launcher-supplied FD from inside a subshell.** Never a raw write
   to a pipe in the shell's own context, and never `trap '' PIPE` as the primary
   defence (it leaks `SigIgn` into children).

Conditions:

5. **Capability records are published, not summarised.** Every `partial:` must
   reach the consumer, because the consumer's correct behaviour depends on it.
6. **The hot path must be a persistent background connection, not
   connect-per-command.** Measured cost of connect-per-command with the daemon
   down: bash p50 1168 µs, zsh p50 1488 µs. That is a per-command tax on every
   shell in the system and is not acceptable as a steady state.
7. **Never fork per command.** A `$(...)` costs ~190 µs; use a compiled emitter
   or write from the shell's own process.
8. **Do not ship a zsh adapter that depends on a `ZDOTDIR` shim as if that were
   the normal case.** Re-test on a distribution-installed zsh before claiming
   the numbers.
