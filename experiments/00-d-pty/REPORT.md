# 0-D — PTY across bash / zsh / fish: report

Run of record: `results/20261123T135441Z/` (driver log:
`results/20261123T135441Z/driver.log`). **49 assertions passed, 0 failed**,
`run.sh` exited 0. Three shells x ten steps = 30/30 steps green.

## 1. Objective

Verify that a supervisor can own a pseudo-terminal, drive an interactive shell
inside it, and read the result back — for `bash` 5.3.20, `zsh` 5.9.2 (local
copy, not installed) and `fish` 4.9.3 — including full-screen programs,
`^C`/`^D`, window resizing with `SIGWINCH`, and several concurrent sessions.

Also establish, with evidence rather than assertion, the mechanism's hard
boundary: a PTY lets you own a **new** terminal session; it does **not** let you
attach to a terminal that already exists.

## 2. Hypothesis

| id | hypothesis | verdict |
|---|---|---|
| H-D1 | `openpty` + `setsid` + `TIOCSCTTY` + `dup2` gives full bidirectional control; command execution is provable by a sentinel | **PASS** |
| H-D2 | prompt/echo handling must be per shell; a literal prompt match is not portable | **PASS** — but the real difficulty was not the prompt, see 5.2 |
| H-D3 | full-screen programs (`vi`, `less`) are unreadable by line scanning; only crash-safe facts are reliable; `top -b -n1` is the plain-text control | **PASS** |
| H-D4 | `TIOCSWINSZ` on the master raises `SIGWINCH` in the slave's foreground pgroup, so `stty size` reports the new geometry with no manual signal | **PASS** |
| H-D5 | sessions held by the same supervisor cannot interfere: own `pts/N`, own session id, own line discipline | **PASS** |
| H-D6 | *(added during the work)* a PTY is a byte pipe, not a terminal; programs that query the terminal must be answered or they stall | **PASS, with a correction** — see 5.3 |

## 3. Method

- Rust crate `li-pty-proto` (deps `nix`, `libc`, `serde_json`, `thiserror`,
  built `--offline`). `cargo test`: **12 unit tests pass**. `clippy -D warnings`
  and `fmt --check`: clean.
- One PTY per session: `openpty` → `fork` → child does `setsid()`,
  `TIOCSCTTY`, `dup2(slave, 0/1/2)`, `chdir`, `execve`. Master side is
  non-blocking + `poll`.
- **No host rc file is ever read.** bash `--norc --noprofile`, zsh `-d` with an
  isolated `ZDOTDIR`, fish `--no-config`; `HOME`, `XDG_CONFIG_HOME`,
  `XDG_RUNTIME_DIR` all point inside `/tmp/li-phase0-d-*`.
- `TIOCSWINSZ` on the master for resizing; `TIOCGWINSZ` to read back.
- The ten steps are identical across shells; only two things are per-shell
  (5.2): the arithmetic command and the zsh module path.
- All records are JSON; `run.sh` splits them into `steps-<shell>.jsonl`,
  `transcript-<shell>.escaped`, `concurrent.jsonl`, `takeover.jsonl` and
  `responder-compare.jsonl`, and asserts every claim with `jq`.

## 4. Raw results

### 4.1 The matrix: 30/30 steps pass

Source: `results/20261123T135441Z/matrix-table.txt`

| step | bash | zsh | fish |
|---|---|---|---|
| 01 shell started | pass | pass | pass |
| 02 sentinel executes | pass | pass | pass |
| 03 pipeline + arithmetic | pass | pass | pass |
| 04 initial winsize | pass | pass | pass |
| 05 resize visible | pass | pass | pass |
| 06 `^C` | pass | pass | pass |
| 07 full-screen `vi` | pass | pass | pass |
| 08 full-screen `less` | pass | pass | pass |
| 09 control `top -b -n1` | pass | pass | pass |
| 10 `^D` exits | pass | pass | pass |

All three shells exited **status 0** after `^D`, and the master reached **EOF**
(`matrix.<shell>.ctrl_d_exit_status = 0`, `master_eof_after_ctrl_d = true`).

### 4.2 What each shell actually emits at startup

`prompt_hex_after_quiet`, first ~20 bytes (`steps-<shell>.jsonl`):

| shell | first bytes | reading |
|---|---|---|
| bash | `1b 5b 3f 32 30 30 34 68 4c 49 2d 42 41 53 48 3e 20` | `ESC[?2004h` (bracketed paste on) then the literal `LI-BASH> ` |
| zsh | `1b 5b 31 6d 1b 5b 37 6d 25 1b 5b 32 37 6d ...` | `ESC[1m ESC[7m % ESC[27m` — zsh paints a reverse-video `%` then blanks the line |
| fish | `1b 5b 3f 75 1b 5b 3e 30 71 1b 5d 31 31 3b 3f 1b 5c ...` | `ESC[?u` (kitty keyboard query), `ESC[>0q`, `ESC]11;?` (bg colour query), then **DCS XTGETTCAP** probes, an alt-screen enter/exit, `ESC[0c` (DA1) |

The zsh and fish lines are the whole story of this experiment: neither prints a
plain prompt, and fish's first output is a **sequence of queries**.

### 4.3 `^C` and `^D`

| shell | `^C` during `sleep 30`: shell alive after | `sleep` completed? | `^D`: exit status | master EOF |
|---|---|---|---|---|
| bash | yes | no | 0 | yes |
| zsh | yes | no | 0 | yes |
| fish | yes | no | 0 | yes |

`^C` was sent as the literal byte `0x03` on the master (not a signal from the
parent), so the interrupt travelled through the line discipline — the real path.
After it, each shell accepted and executed a fresh sentinel command, which is
the assertion, not the absence of an error message.

### 4.4 Window resize

`TIOCSWINSZ(50, 200)` on the master, then `stty size` typed into the shell
(`05_resize_visible`), for all three shells:

| shell | `TIOCGWINSZ` rows/cols | child reported new size |
|---|---|---|
| bash | 50 / 200 | `true` |
| zsh | 50 / 200 | `true` |
| fish | 50 / 200 | `true` |

No manual `kill -WINCH` anywhere in the code path: the kernel sends `SIGWINCH`
to the slave's foreground process group when `TIOCSWINSZ` changes the geometry.

### 4.5 Full-screen programs vs. the batch control

| shell | `vi` alt-screen in/out | `vi` shell usable after quit | `less` alt-screen in | `less` plain text visible | `top -b` PID header |
|---|---|---|---|---|---|
| bash | yes / yes | yes | yes | yes (`root:`) | yes |
| zsh | yes / yes | yes | yes | yes | yes |
| fish | yes / yes | yes | yes | yes | yes |

`vi` produced 5042 bytes of screen writes for a one-line file
(`07_fullscreen_vi.screen_bytes`; 5044 for zsh, 5250 for fish — the byte count
depends on the prompt redraws interleaved with the screen), and `less` 2585
bytes: `ESC[?1049h`, cursor
addressing, `ESC[2J`, `ESC[K`. `top -b -n1` produced 55 306 bytes of **ordinary
line output** with a real `PID ... COMMAND` header row (byte counts vary run to
run with process load, and so do the `/dev/pts/N` numbers; the format, the
assertions and the verdicts do not).

`vi` was started as `vi -n <workdir>/vi-target.txt`. `-n` matters: without it a
stale `/tmp/.li-phase0-vi.txt.swp` from an earlier interrupted run made `vim`
stop at its `E325: ATTENTION ... Swap file already exists` dialog, and every
subsequent keystroke went to that dialog instead of to the shell. Recorded in
5.4 because it silently poisoned four steps.

### 4.6 Four concurrent sessions, no cross-talk

`results/20261123T135441Z/concurrent.jsonl`, 4 sessions in one process, mixed
shells, each with its own token and its own window size:

| session | shell | pty | own token seen | window size | foreign tokens |
|---|---|---|---|---|---|
| 0 | bash | `/dev/pts/23` | 2 | 24 80 | none |
| 1 | zsh | `/dev/pts/25` | 2 | 25 81 | none |
| 2 | fish | `/dev/pts/24` | 4 | 26 82 | none |
| 3 | bash | `/dev/pts/26` | 2 | 27 83 | none |

- 4 distinct slave paths, 4 distinct pids, 4 distinct geometries, each
  confirmed by `stty size` inside the session.
- Every session's token appears only on its own stream: `no_foreign_tokens = 0`
  and `no_cross_talk = true`.
- fish shows 4 occurrences of its token instead of 2 because fish redraws its
  own input line on every keystroke; the extra occurrences are redraws, not
  extra executions, which is why the assertion counts *bare output lines* and
  not raw substrings.

### 4.7 The responder: measured necessity

Source: `results/20261123T135441Z/responder-compare.txt`. Same handshake, same
environment; the only variable is whether terminal queries are answered.

| shell | responder | queries seen | queries answered | sentinel output arrives | handshake ms |
|---|---|---|---|---|---|
| bash | ON | 0 | 0 | yes | 49 |
| bash | OFF | 0 | 0 | yes | 49 |
| zsh | ON | 0 | 0 | yes | 49 |
| zsh | OFF | 0 | 0 | yes | 49 |
| fish | ON | **12** | **12** | yes | **49** |
| fish | OFF | **12** | 0 | yes | **10 064** |

### 4.8 The takeover boundary, with evidence

`results/20261123T135441Z/takeover.jsonl`:

```json
{"foreign_terminals_found": 12,
 "sysctl": {"dev.tty.legacy_tiocsti": "0",
            "kernel.yama.ptrace_scope": "1"},
 "our_own_pty_probe": {
   "open_foreign_slave": "ok",
   "TIOCSTI": "denied: EIO: I/O error",
   "TIOCSCTTY_force": "denied: EPERM: Operation not permitted"}}
```

and the first two foreign processes it found:

```json
{"pid":3683,"comm":"fish","fd0":"/dev/pts/0","tty_nr":"34816","owns_this_terminal":false}
{"pid":3815,"comm":"miyu","fd0":"/dev/pts/0","tty_nr":"34816","owns_this_terminal":false}
```

Independent cross-check (`results/.../ps-terminals.txt`) showing that the
terminals this experiment did not create belong to other sessions:

```
PID      PPID     SID      TTY        COMMAND
3683     3683     3683     pts/0      fish
50211    940      50211    pts/2      fish
```

The critical detail: opening another session's `/dev/pts/N` **succeeds**
(`open_foreign_slave: ok`) because the devpts node is a plain file to us. Being
able to *open* it buys nothing — every mechanism that would let bytes or signals
reach the other session's foreground process is refused.

### 4.9 Leak check

`results/20261123T135441Z/leaks.txt`: no session, no `vim`, and no child of our
work directory survived the run.

## 5. Analysis

### 5.1 Why the PTY mechanism works at all

`setsid()` gives the child a session with no controlling terminal; `TIOCSCTTY`
then makes the slave **the** controlling terminal; `dup2` makes 0/1/2 all the
slave. After that the slave has a line discipline: `ISIG` turns a `0x03` byte
into `SIGINT` for the foreground pgroup, `ICANON` handles `0x04` as EOF, and
`TIOCSWINSZ` generates `SIGWINCH`. That is why 4.3 and 4.4 work with raw bytes
and no signal injection from the parent — the kernel is doing the work, which is
exactly what makes the mechanism trustworthy.

### 5.2 The per-shell difficulties, in the order they bit

**(a) fish has no `PS1`, and no static prompt at all.** It builds the prompt
from the `fish_prompt` function and repaints the command line on every
keystroke, emitting `ESC[<n>C` between individual characters. Consequence: the
literal token `LI_SENTINEL_FISH` **never appears contiguously** in the raw
stream. Measured: `occurrences_raw = 0` while the command demonstrably ran.

The portable signal turned out to be neither the prompt nor a substring, but
**"a printed line that is exactly the token"** — the command's own output always
forms such a line, while the terminal echo (a redraw, not a print) never does.
This is `util::count_bare_lines`, unit-tested against a bracketed-paste stream
(`bare_lines_survive_bracketed_paste_noise`).

**(b) zsh paints `%` in reverse video and uses `PROMPT_SP`.** With `PROMPT_SP`
(default), zsh emits a reverse-video `%`, a long run of spaces, a `\r`, and then
the prompt — so output and prompt end up on the same physical line. A naive line
split then decides that `top` printed nothing.

**(c) Resyncing between commands is mandatory.** After a command the shell
rejects, the text stays in its editing buffer, and the next `send_line` is
*appended* to it. Measured with fish: one rejected command turned the next eight
steps into a single garbage line. Fix: `^C` + `^U` + settle before each step.
This is why step 3 of the first runs failed for reasons that had nothing to do
with step 3.

**(d) The arithmetic probe was the only genuinely non-portable command.** My
first version used `echo [$((2 + 3))]`. Measured results: zsh →
`zsh: no matches found: [5]` (the `[5]` is a glob), fish →
`fish: command substitutions not allowed in command position`. Fix: per-shell
`echo $((2 + 3))` / `math 2 + 3`, and then wait for the bare line `5`.

**(e) The local zsh copy could not load its own modules.** `zsh -f -i` printed
`zsh: failed to load module 'zsh/zle': /usr/lib/zsh/5.9.2/zsh/zle.so: cannot
open shared object file`. The modules *are* present, just not where a
`/usr/lib`-compiled zsh looks. Fix: `-d` plus an isolated `ZDOTDIR` whose
`.zshenv` sets `module_path` to
`experiments/toolchain/root/usr/lib/zsh/<ver>`. This is the only reason zsh
works here, and it is a property of the unpacked copy, not of zsh.

`-f` cannot be combined with this fix (`-f` also skips the shim), so the
isolation is `-d` + `ZDOTDIR` + no `/etc/zsh/*`: no host rc file is read.

### 5.3 The responder is the real finding, and my first hypothesis was wrong

fish's startup stream is a list of terminal queries: kitty-keyboard `ESC[?u`,
`ESC[>0q`, background colour `ESC]11;?`, **XTGETTCAP** DCS probes, `ESC[0c`. A
PTY is a byte pipe: nothing on the other end answers them.

My first hypothesis was that fish would **block forever** without a responder.
**That was wrong, and the measurement says so.** With the responder disabled
fish still ran the command (`sentinel_output_seen: true`) — it simply took
**10 064 ms** instead of **49 ms** (4.7), i.e. it times out and proceeds.

What the responder actually buys is therefore *latency and determinism*, not
correctness: 200x faster startup, and — the part that matters — no dependence on
a shell's internal timeout. Whether a *different* program blocks indefinitely
is **NOT TESTED** (6.1).

`responder.rs` answers the queries real tools send: DSR (`ESC[6n`), DA1/DA2
(`ESC[c`, `ESC[>c`), kitty keyboard (`ESC[?u` → "no support"), XTVERSION,
text-area size (`ESC[18t`), `OSC 10/11;?` colour queries, and XTGETTCAP with a
well-formed "not found". Each answer is recorded, which is how 4.7 knows fish
asked exactly 12 questions.

### 5.4 Two traps that produced false results before being caught

1. **`vi` + a stale swap file.** `vim` stopped at `E325 ... Swap file already
   exists` and swallowed every later keystroke. Four steps reported
   `shell_usable_after_quit: false` for a reason that had nothing to do with
   PTYs. Fixed with `-n` and a per-run path.

2. **`responder-off` initially proved nothing**, because it also changed the
   environment (the zsh shim was skipped, zsh then exited 127), and because
   `read_until_any` matched the *echo* of the typed token rather than its
   output. Fixed by (a) resolving the environment through the same `resolve_env`
   as the matrix and (b) asserting on bare printed lines. The pre-fix numbers are
   not in the report of record; the reported ones come from the ON/OFF
   comparison.

### 5.5 The takeover boundary: mechanism, not configuration

A supervisor can do three things with a terminal it did not create:

| attempt | result | why |
|---|---|---|
| open `/dev/pts/N` | **allowed** | it is just a device node with mode 600 and we are the owner |
| `TIOCSTI` (inject bytes into its input queue) | **denied: EIO** | `dev.tty.legacy_tiocsti = 0` on this kernel |
| `TIOCSCTTY` with force=1 (become its controlling terminal) | **denied: EPERM** | the fd is not part of our session; the kernel also requires the caller to be a session leader without a ctty |

So `owns_this_terminal` is false for every one of the 12 foreign terminals
discovered. There is no partial capability to be "worked around": a PTY you did
not create is **readable as a file and inert as a terminal**.

This is the boundary that matters for the product: observing an *existing* user's
terminal requires cooperation from the process running in it (a shell hook —
experiment 0-A — or a privileged agent), never PTY acquisition.
`/proc/<pid>/fd/0 -> /dev/pts/0` names the terminal; it does not grant access.

### 5.6 What full-screen programs permit

For `vi` and `less` the reliable facts are: the process is alive, the
alternate-screen sequence appeared, the process exited, and the shell was usable
afterwards. The *content* is not recoverable without a terminal emulator: 5042
bytes of cursor-addressed writes for a file containing one line. `top -b -n1` is
the control that proves the difference is the program's behaviour (line output,
55 306 bytes, readable header) and not the PTY.

## 6. Known limitations

**NOT TESTED:**

1. **NOT TESTED: whether any program blocks *indefinitely* without a
   responder.** Only fish was measured, and it recovers after ~10 s. vim/less ran
   fine with the responder already in place, so their behaviour *without* it is
   unmeasured.
2. **NOT TESTED: a real terminal emulator / screen reconstruction.** Only the
   presence of escape sequences and process-level facts is recorded. No claim is
   made about what a user would have *seen*.
3. **NOT TESTED: `nu`, `dash`, `tcsh`, `ksh`.** bash, zsh and fish only.
4. **NOT TESTED: attaching to an existing session under any privilege model.**
   Only the unprivileged case was measured; whether root or a `CAP_SYS_ADMIN`
   process could take over a terminal is out of scope (and unmeasurable here: no
   sudo).
5. **NOT TESTED: `TIOCSTI` on a kernel with `legacy_tiocsti=1`.** The refusal in
   5.5 is a sysctl-dependent result, not a fundamental one, and is reported as
   such.
6. **NOT TESTED: more than 4 concurrent sessions**, or sustained load.
7. **NOT TESTED: byte-exact replay of a session** (no timing capture, so
   `less`/`vi` streams cannot be replayed at the right speed).
8. **NOT TESTED: signals other than `^C`/`^D`** (`^\`, `^Z`, job control).
9. **NOT TESTED: non-UTF-8 payloads** through the PTY; the harness decodes
   lossily.
10. **Timing caveat.** The 49 ms handshake figure includes the harness's own
    `wait_quiet` settle before the command, so it is an upper bound on the
    responder's effect, not a measurement of it alone.
11. **zsh here is a special case.** All zsh results depend on the `ZDOTDIR` shim
    (5.2e). A system-installed zsh would not need it; results may differ for a
    `zsh` whose modules load normally.

**`unsafe` justification** (6 blocks, all in `pty.rs`):

- `fork` — unavoidable for PTY ownership; the child branch calls only
  async-signal-safe functions (`setsid`, `dup2`, `close`, `chdir`, `execvpe`,
  `_exit`) and never returns into Rust.
- `TIOCSCTTY`, `TIOCSWINSZ`, `TIOCGWINSZ`, `TIOCSTI` — ioctls, with no suitable
  safe wrapper in `nix` 0.29 for these argument types.
- `fcntl` for `O_NONBLOCK` — kept off the `nix` feature set to avoid pulling in
  `fs`.

Each is annotated `// SAFETY:`. `panic = "abort"` in release.

**Deviations from protocol:** none. No host rc file modified, no `sudo`, no
recursive deletion; everything written under this experiment directory and
`/tmp/li-phase0-d-*`. Two processes from an interrupted earlier run (leaked
`vim` instances holding a stale swap file) were killed by pid with `kill -9`;
they were mine, and no file was deleted.

## 7. Conclusion

**PASS.**

All five original hypotheses hold, 30/30 matrix steps are green, and 49
assertions pass with zero failures. Interactive shell control through an owned
PTY is reliable for bash, zsh and fish — including `^C`, `^D`, `TIOCSWINSZ`/
`SIGWINCH`, full-screen programs and 4-way concurrency with no cross-talk.

H-D2 needed re-framing: the difficulty was never prompt matching, it was (a)
fish redrawing its input line character by character, (b) zsh's `PROMPT_SP`
joining the prompt to the previous output, (c) the need to resync a shell's
editing buffer between commands, and (d) fish's startup terminal queries. H-D6
was **added and corrected** mid-experiment: the responder is required for speed
and determinism (10 064 ms → 49 ms), not for correctness, because my "it blocks
forever" hypothesis was falsified by measurement.

The takeover boundary is confirmed as a mechanism limit with three independent
pieces of evidence (open succeeds, `TIOCSTI` denied, `TIOCSCTTY` denied).

## 8. Gate

**May Phase 1 implement this? YES, with conditions.**

Adopt as-is:

1. `openpty` → `setsid` → `TIOCSCTTY` → `dup2` → `execve` as the only session
   bootstrap. It is exercised 30 times per run across three shells.
2. **Ship the terminal-query responder as a first-class component**, not a test
   helper. Without it every fish start costs 10 s (4.7). It must cover at
   minimum DSR, DA1, kitty-keyboard, `OSC 10/11;?`, `ESC[18t` and XTGETTCAP, and
   it must *record* what it answered so a shell's behaviour can be explained
   later.
3. Drive every session with the per-shell policy table (5.2): reset the editing
   buffer between commands, never match a literal prompt, and assert on bare
   printed lines.
4. Treat "PTY ownership" and "terminal access" as separate capabilities in the
   product model. 5.5 shows the second is unavailable without in-process
   cooperation.

Conditions before Phase 1 depends on it:

5. **Resolve shell modules explicitly.** The local zsh needed a `ZDOTDIR` shim
   for `zle.so`. Any distribution that relocates modules will break silently (a
   dead ZLE looks like "the shell ignores input"). Phase 1 must detect this and
   fail loudly.
6. **Do not promise screen content.** A terminal emulator (or a decision to
   record raw streams and let the UI decode them) is a separate, sizeable work
   item. Phase 1 can honestly promise: session control, exit statuses, output
   bytes, resize, and the fact that a full-screen program ran.
7. **Measure the responder against vim/less/top**, and against a program known
   to block indefinitely, before declaring the query set complete. Only fish was
   characterised here (6.1).

> **路径替换说明**：本次公开前对 `results/` 与脚本里的绝对路径做了字符串替换，换成两个占位符 `<WORKSPACE>` 与 `<HOME>`。
> 规则、未改动的量、以及原始形态见 [`../SCRUB-NOTICE.md`](../SCRUB-NOTICE.md)。
