# 0-A — Interactive shell hook capability (bash / zsh / fish)

Host: Arch Linux, kernel 7.2.8-zen, x86_64. Shells: bash 5.3.20, zsh 5.9.2
(local unpacked copy at `experiments/toolchain/root/usr/bin/zsh`, not installed),
fish 4.9.3. Everything was measured through a real pty by
`harness/drive.py`; hooks wrote one-line JSON to FD 3 so hook output and
terminal noise never mixed.

## 1. Objective

Decide from measurement whether an unpatched interactive shell can be a reliable
source of `CommandStarted` / `CommandFinished` events, and at what cost. Deliver
a minimal working adapter per shell, a machine-readable capability record per
shell, and a 13-case × 3-shell behaviour matrix.

## 2. Hypothesis

See `README.md` §Hypothesis. In short: H-A1 bash must use `PROMPT_COMMAND` +
`history 1` (PS0 lags one command); H-A2 the `DEBUG` trap must be rejected;
H-A3 zsh `preexec`/`precmd`; H-A4 fish `fish_preexec`/`fish_postexec`; H-A5 a
dead sink must cost about the same as `/dev/null`; H-A6 hooks must not fire in
non-interactive shells.

## 3. Method

- `harness/drive.py` — pty driver, isolated configs only
  (`bash --norc`, `zsh -f`, `fish --no-config`), FD 3 for hook JSON.
- `harness/probes.py` — 10 mechanism probes, each answering one question.
- `harness/cases.py` — the 13 cases × 3 shells, replaying fixed step scripts.
- `harness/analyze.py` — reduces raw event files to `results/analysis.json`.
- `harness/latency.py` — prompt-to-prompt latency, 4 sink modes, 3 independent
  sessions × 60 iterations, with a `--selftest` that proves the detector waits
  for the command (a `sleep 0.5` must report ≥ 500 ms).
- `harness/adapters_latency.py` — the same, for the **shipped adapters**.
- `harness/hookcost.py` — cost of the hook body alone, timed from inside the
  shell, excluding pty round-trip.
- `harness/noninteractive.sh` — negative control (8 invocations).
- `run.sh` — regenerates everything; exits non-zero on any sanity failure.

No host rc file was touched; no `sudo`; nothing installed system-wide.

## 4. Raw results

### 4.1 Mechanism probes (`results/probe-*.log`)

| Probe | Finding | Evidence |
|---|---|---|
| PS0 vs PROMPT_COMMAND | PS0 sees the **previous** `$?` and `$BASH_COMMAND`; `PROMPT_COMMAND` sees the real status, `history 1` the real text | `probe-bash-ps0-vs-promptcommand.log` |
| PS0 subshell | `$( )` and `${v:=w}` in PS0 do not persist; an assignment inside an **arithmetic subscript** does, while expanding to nothing | `probe-bash-ps0-subshell.log` |
| PS0 fire count | exactly 1 per executed line, **0** for a blank line, 1 for a loop/pipeline | `probe-bash-ps0-count.log` |
| dead pipe | raw `printf >&3` to a closed pipe **kills the shell** (SIGPIPE, sig 13) | `probe-bash-sigpipe.log` |
| `trap '' PIPE` | SIGPIPE ignore is inherited by children (`SigIgn` 0x0 → 0x1000) | `probe-bash-trap-pipe-inherit.log` |
| fork cost | parameter expansion 1 µs, `builtin history 1 >/dev/null` 3-7 µs, any `$( )` ~200-260 µs | `probe-fork-cost.log`, `probe-fork-cost-extclock.log` |
| zsh clock | `EPOCHREALTIME` has a **10-digit** fraction; naive subtraction overflows 64-bit ("number truncated") | `probe-zsh-overflow.log`, `probe-clock-precision.log` |
| fish in a pty | with a real `TERM` fish blocks on DA1/OSC11 queries and never prompts; `TERM=dumb` works | `probe-fish-term.log` |
| unix sockets | no shell can `> /path.sock` (ENXIO); only zsh `zsocket` can dial | `probe-unix-socket-open.log` |
| DEBUG trap | fires per **simple** command: 2 for a pipeline, 3 for `a;b;c`, 6 for a 3-iteration loop, 0 inside `( )` | `probe-bash-debugtrap.log` |

### 4.2 Hook-body cost, timed inside the shell (`results/latency/hookcost.json`)

| shell | hook body median | method |
|---|---|---|
| bash | **3 µs** | per-command `EPOCHREALTIME` |
| zsh | **2348 µs** | per-command `$(_li_now)` |
| fish | **1085 µs** | batch loop, `date` amortised |

### 4.3 Prompt-to-prompt latency, by sink mode (`results/latency/latency.json`)

Median of 3 independent sessions, n=180 per cell, microseconds.

| shell | empty hook | → /dev/null | → dead socket | → live socket |
|---|---|---|---|---|
| bash | 152 (p99 395) | **193** | **1148** | 1140 |
| zsh | 166 (p99 454) | **230** | **1462** | 1456 |
| fish | 918 (p99 2039) | **372** | **372** | 362 |

Reading, fish caveat: fish's `empty` cell (918 µs) is *larger* than its
`devnull` cell (372 µs). That is not a hook cost. Measured directly, fish with
**no hook at all** already takes ~1.8-2.1 ms per prompt cycle in this pty, and
an empty or writing handler is indistinguishable from that within noise
(`results/probe-fish-prompt-cost.log`). So the fish row must be read as
"fish is intrinsically ~2 ms per prompt and the hook adds nothing measurable",
not as a comparison of sinks.

Reading: the "dead socket" column forks `harness/li_emit`, which fails to
`connect()` at once. It costs the same as a *live* socket (bash 1148 vs 1140,
zsh 1462 vs 1456) — i.e. **the daemon being down is not what costs time; the fork
is.**

### 4.4 Latency of the real adapters (`results/latency/adapters.json`)

Microseconds, median of 3 sessions of 40 commands.

| shell | hooks off | adapter, FD3 sink dead | adapter, FD3 sink live | adapter, no FD3 |
|---|---|---|---|---|
| bash | 160 | **2760** | 2808 | 3539 |
| zsh | 311 | **3327** | 3028 | 3063 |
| fish | 1848 | **3340** | 3408 | 3475 |

The `fd3dead` and `fd3ok` columns agree within noise for every shell: **an
unreachable sink costs the shell nothing extra.**

### 4.5 Negative control (`results/noninteractive.log`)

9 invocations across `bash -c`, `bash script`, `bash -i -c`, `zsh -f -c`,
`zsh -f script`, `zsh -f -i -c`, `fish -c`, `fish script`: **0 bytes** written to
FD 3 in every case, 0 `HOOK_FIRED` verdicts.

### 4.6 The 13 × 3 matrix

Each cell: what the raw event stream showed. Evidence for cell (case, shell) is
always the pair `results/<case>.<shell>.events.jsonl` and
`results/<case>.<shell>.term.log`, e.g. `results/t07.zsh.events.jsonl`; all 39
pairs exist (see `results/analysis.json`).

| # | case | bash | zsh | fish |
|---|---|---|---|---|
| t01 | normal command | ✅ `echo t01-normal` rc 0 | ✅ same | ✅ same (+`source` line, +`exit`) |
| t02 | backslash continuation | ⚠️ joined: `echo t02-one two` | ✅ verbatim: `echo t02-one \⏎two` | ⚠️ mangled: `echo t02-one \two` |
| t03 | pipeline | ✅ 1 event, whole pipeline | ✅ 1 event | ✅ 1 event |
| t04 | `&&` `\|\|` `;` | ✅ 4 events, 1 per line | ✅ 4 | ✅ 4 (`and`/`or` forms) |
| t05 | builtins cd/export/echo | ✅ cwd tracks `cd` | ✅ | ✅ (`set -gx` form) |
| t06 | function def + call | ✅ 2 events, call reported as typed | ✅ | ✅ |
| t07 | alias | ✅ reports `t06alias`, *not* the expansion | ✅ same | ✅ same |
| t08 | background `&` | ⚠️ launch reported, completion not | ⚠️ same | ⚠️ same |
| t09 | subshell + `$( )` | ✅ whole line, one event, substitutions invisible | ✅ same | ✅ same (`begin/end`, `( )`) |
| t10 | Ctrl+C | ✅ `sleep 30` → rc **130** | ✅ 130 | ✅ 130 |
| t11 | shell SIGKILLed | ✅ **no unclosed event** (last event is the previous command) | ✅ | ✅ |
| t12 | large output (20 000 lines) | ✅ 3 events, no loss | ✅ | ✅ |
| t13 | ten sequential echoes | ✅ 10/10, order preserved, `sequence` monotonic | ✅ 10/10 | ✅ 10/10 |

Counts behind the matrix (`results/analysis.json`), user events / all events:
bash and zsh are identical for t01–t13; fish always has +1 (`source` line) and
+1 more where `exit` is typed, because fish hooks exist the moment the file is
sourced and fish also reports `exit`.

## 5. Analysis

**The three shells need three different mechanisms, and only one of them is
cheap.**

*bash* — `PROMPT_COMMAND` + `history 1` is the only source of both the real exit
status and the real command text; PS0 cannot serve as the event source because it
lags one command (F1) and cannot carry state out of its subshell except through
an arithmetic subscript (F2). The resulting hook body costs **3 µs**, because the
only fork is the one `history 1` call, and bash can format time with the
`printf '%(...)T'` builtin. bash is the best-behaved of the three.

*zsh* — `preexec`/`precmd` is the cleanest *semantic* model: `preexec` gets the
command line verbatim (newlines intact, t02), `precmd` gets the true `$?`. But it
costs **2348 µs** to run, because a start timestamp has to come from `$( )`
(zsh has no fork-free sub-second clock builtin here) and the event line is built
with more substitutions than bash's. zsh also needs two host-specific fixes:
`module_path` (else `zsh/datetime` fails) and a 6-digit truncation of a 10-digit
`EPOCHREALTIME` fraction (else 64-bit overflow).

*fish* — `fish_preexec`/`fish_postexec` is the most *convenient*: the command
line arrives in `$argv`, the status in `$status`, and the duration for free in
`$CMD_DURATION`. It costs **1085 µs** of hook body, and fish's own prompt costs
~1 ms before any hook is installed (the `empty` column in 4.3 is 1079 µs vs
bash's 202 µs). So fish is expensive with or without us.

**The DEBUG trap is rejected, with counts.** See `probe-bash-debugtrap.log`:
- one typed line produced **2** firings for `printf ... | wc -l`, **3** for
  `echo A; echo B; echo C`, **6** for `for i in 1 2 3; do echo L$i; done`;
- it fired **0** times inside `( echo S1; echo S2 )`, so subshells are invisible
  while the enclosing line is visible — event counts cannot be reconciled with
  typed lines;
- it interleaves with `set -x`, and both streams become unreadable while `-x` is
  on.
There is no rule that maps firings back to one user command without extra
bookkeeping that `history 1` gives for free. Rejected.

**The most important engineering result is negative: a hook can kill the shell.**
A raw write to a pipe whose reader died terminates interactive bash with SIGPIPE
(`probe-bash-sigpipe.log`). Three mitigations work: `trap '' PIPE` around the
write (costs 6 µs, and the ignore flag leaks into children while installed, F4),
writing from a subshell, or a `connect()` that fails. The adapters use the
first, scoped to the single write, plus a one-shot capability probe at load that
silently disables emission if FD 3 is unusable.

**Sink availability is not a latency problem.** 4.3 and 4.4 show dead and live
sinks within noise. What *does* cost time is forking a helper: ~600-900 µs per
command. That is why the adapters write to an inherited FD 3 rather than dialling
the daemon themselves — and they could not dial anyway, since no shell can open
an AF_UNIX socket by redirection (F7).

**Two diagnoses in this experiment were wrong and are recorded as such** (F11
`tty`, F14 `add-zsh-hook`). Both were caused by the same class of bug: the
harness, not the shell, was broken. The lesson kept in the report is that a "no
events" symptom here had at least four independent causes (fpath, module_path,
`dup2` fd clobbering, payload quoting).

## 6. Known limitations

- **Not tested / NOT TESTED**: nushell (not installed); zsh `zsocket`-based
  dialling (module not exercised end-to-end); fish SIGPIPE resilience with a
  dead FD 3 (fish may trap SIGPIPE itself — `capabilities/fish.json` marks it
  `unknown`); multi-user or SSH-tunnel scenarios; shells other than the exact
  versions above; macOS/BSD.
- **Not reported by any shell**: the `source adapters/...` line itself in bash
  and zsh (hooks install during it), and `exit` in bash/zsh. fish reports both,
  which is why raw event counts are not comparable across shells.
- **No `CommandStarted` with a monotonic clock.** bash and zsh expose wall clock
  only; `ts_mono_ns` from the protocol envelope is **not** emitted by any
  adapter. Duration is wall-clock and will skew across NTP steps or suspend.
- **Background jobs** are reported at launch only; completion is never observed.
- **`history 1` is a text channel**: with `HISTCONTROL`/`HISTIGNORE` set to drop
  a line, bash reports `command: null` with `command_text_source:"unavailable"`
  rather than guessing. That path is implemented but only lightly exercised.
- **fish's `$CMD_DURATION`** is integer milliseconds, too coarse to measure the
  hook itself; fish hook cost in 4.2 comes from a batch loop, not per command.
- **`trap '' PIPE` leaks** SIGPIPE-ignore into user commands for the duration of
  the write (microseconds). A user command that itself relies on SIGPIPE
  semantics could in principle observe it; not tested.
- **pty measurements include terminal paint time**, which is why prompt-to-prompt
  numbers (2.7 ms for bash with hooks) are far larger than the 3 µs hook body.
  Do not quote 4.3 as "the hook costs 2.7 ms"; that gap is the harness.

## 7. Conclusion

**PASS-WITH-LIMITS.**

All 13 cases were executed on all 3 shells (39/39 recorded events, 0 NOT
TESTED). Command text, exit status, cwd, pid/sid/new-session, Ctrl+C status (130)
and "no unclosed event after SIGKILL" all verified on all three shells. Multi-line
fidelity is where they diverge, and bash loses information. The engineering
constraint that matters most — a hook must not be able to kill the shell — was
found by measurement and is handled explicitly. The cost picture is honest: bash
is effectively free (3 µs), zsh and fish each add ~1-2 ms per command.

Limits that keep this from a clean PASS: no monotonic clock in any shell; zsh and
fish hook bodies are expensive; fish was not tested for dead-sink resilience; and
the `source`/`exit` reporting asymmetry means a consumer must filter by command
type before counting events.

## 8. Gate

**May Phase 1 implement a Shell Adapter baseline? YES, for bash and zsh; fish
with the caveats below.**

Approved baseline combinations:

| shell | approved hook combination | condition |
|---|---|---|
| bash | `PROMPT_COMMAND` + `builtin history 1`, PS0 only as an optional start tick | mandatory `trap '' PIPE` around every sink write |
| zsh | `preexec` + `precmd` appended directly to the `*_functions` arrays | mandatory 6-digit fraction truncation; `module_path` fix when zsh is not installed |
| fish | `fish_preexec` + `fish_postexec` | sink writes must be guarded; fish's dead-sink behaviour is `unknown` |

Non-negotiable conditions for Phase 1:

1. **The sink must be an inherited FD, never a shell-opened socket.** No shell
   can open an AF_UNIX stream socket by redirection (ENXIO on all three). The
   launcher opens the socket and passes FD 3.
2. **Every sink write must be SIGPIPE-proof.** `trap '' PIPE` around the write,
   or a subshell, and a load-time probe that disables emission permanently once
   a write fails. A daemon restart must never be able to kill a user's shell.
3. **Do not promise a monotonic timestamp from a shell hook.** Emit wall clock
   and let the daemon stamp `ts_mono_ns` on receipt; mark shell-derived times
   `evidence_status: "declared"`, as every adapter here does.

Capabilities downgraded to `unknown` (do not build on them):
`fish.dead_sink_resilience`, `fish.monotonic_clock`, `zsh.zsocket_dialling`,
`*.background_completion`, `*.monotonic_clock`, `*.internal_script_commands`.
Set to `false` (verified absent): `fires_in_noninteractive`,
`fires_in_dash_c`, `sink_can_be_unix_socket_redirection`, `event_on_kill9`,
`unclosed_event_on_kill9`.

Rejected approaches and why: **bash DEBUG trap** — fires per simple command (2,
3, 6 firings for one line; 0 inside subshells) and destroys `set -x` legibility.
**PS0 as the event source** — lags one command and cannot carry state out of its
subshell. **Self-dialling from the hook** — impossible without an extra
executable, which costs ~600-900 µs per command.

> **路径替换说明**：本次公开前对 `results/` 与脚本里的绝对路径做了字符串替换，换成两个占位符 `<WORKSPACE>` 与 `<HOME>`。
> 规则、未改动的量、以及原始形态见 [`../SCRUB-NOTICE.md`](../SCRUB-NOTICE.md)。
