# Shell adapters (P1-16, P1-17, P1-18) and the two open design questions

Each adapter emits a **request** line, not a protocol frame:

```
{"type":"CommandFinished","ts_wall":"...","payload":{...}}
```

`li-hook` adds `event_id`, `session_id` and `sequence`. Reason (C9): a valid
envelope needs three pieces of state a shell cannot hold for free, and every
`$( )` in a hook body costs 200–350 µs (0-A `probe-fork-cost.log`).

| shell | mechanism | rejected | fork/command |
|---|---|---|---|
| bash | `PROMPT_COMMAND` + `builtin history 1`; PS0 only as a start tick | `DEBUG` trap (2/3/6 firings per line, 0 inside `( )`, breaks `set -x`); PS0 as source (lags one command) | 1 |
| zsh | `preexec`/`precmd` appended to the raw `*_functions` arrays | `add-zsh-hook` on an already-defined function (marks it an autoload stub and **erases** it, 0-A F14) | 0 |
| fish | `fish_preexec`/`fish_postexec`, `$CMD_DURATION` as duration | `fish_prompt` hook (fires per repaint, not per command) | 1 |

zsh also needs its 10-digit `EPOCHREALTIME` fraction truncated before any
arithmetic, or `${EPOCHREALTIME/./}` subtraction overflows 64-bit and yields
garbage durations with no error (`probe-zsh-overflow.log`).

## C10 — what "inherited FD" still needs

0-A proved no shell can open an `AF_UNIX` stream socket by redirection (`ENXIO`
on all three), so the sink must be launcher-supplied. Each adapter probes fd 3
once and silences itself permanently if the probe fails. The gap to P1-08:

1. **No launcher exists in this batch.** `li run` is P1-20 and `li-daemon` is
   P1-11, so today a real user's shell gets `--sink=socket`, the shape C10
   rejects. The inherited path is implemented and tested (`tests/no_daemon.rs`
   drives fd 3 through a wrapping `sh`), but nothing in production produces fd 3.
2. **An inherited fd cannot carry `SO_SNDTIMEO` without `unsafe`.** Timeouts need
   a `RawFd` we own (`FromRawFd`), which the workspace forbids. `li-hook`
   substitutes one writer thread the loop waits on for 5 ms. That bounds the loop
   but leaks the thread if the kernel write never returns, so **C1 is met
   literally only on `--sink=socket`**.
3. **`SigIgn` leakage is scoped, not absent.** `trap '' PIPE` exists for the
   duration of one write. A user command running between install and removal
   could observe it; the window is microseconds and its user-visible effect is
   untested (0-A §6 says the same).

## P1-10 — `panic = "abort"` is not expressible

Cargo cannot set `panic` for one package. Measured on a scratch workspace:
`cargo build` fails with "`panic` may not be specified in a `package` profile",
and `[profile.release.package.li-hook] opt-level = 3` is accepted where adding
`panic = "abort"` is not. The Cargo book (Profiles → Overrides) states overrides
"cannot specify the `panic`, `lto`, or `rpath` settings". Options, none of which
this batch may choose alone:

* global `[profile.release] panic = "abort"` — changes every crate, needs sign-off;
* a second workspace holding only `li-hook` — duplicates the build and breaks
  `cargo test --workspace`;
* drop the requirement — `li-hook` has no panic path that matters (no `unwrap` on
  input, no indexing, every error returned), so abort only makes a crash quieter.

## P1-09 — SIGPIPE

0-A `probe-bash-sigpipe.log`: a raw `printf >&3` to a closed pipe **kills**
interactive bash with `sig 13`. All three adapters carry both defences: the write
is wrapped in `trap '' PIPE`, removed immediately so `SigIgn` is not inherited
(`probe-bash-trap-pipe-inherit.log`, `0x0 → 0x1000`), and a load-time probe sets a
latch that stops emission for the rest of the shell's life after one failure.
`tests/no_daemon.rs` proves it with a real interactive shell and a pipe whose
reader is killed.

## What this batch measured (2026-11-24)

Adapter cost per command, as a **difference** against the same pty with no adapter
(`cargo test -p li-hook --test hook the_adapter_cost -- --nocapture`):

| shell | with adapter | without | delta |
|---|---|---|---|
| bash | 2 187 906 ns | 113 151 ns | **2 074 755 ns** |
| zsh | 2 438 770 ns | 171 735 ns | **2 267 035 ns** |
| fish | 15 955 581 ns | 14 813 543 ns | **1 142 038 ns** |

These are **not** comparable to 0-A §4.2's hook-body-only figures (bash 3 µs, zsh
2348 µs, fish 1085 µs). This number includes the pty round trip, which 0-A §6 warns
is one to three orders of magnitude larger than the body, and it includes the
`script(1)` wrapper. It is what the harness can see, and it agrees with 0-A §4.4's
adapter-level 2760/3327/3340 µs in order of magnitude.

## The receiver's gap formula cannot see a tail drop

Measured, and pinned by `a_tail_drop_is_invisible_to_the_receiver_...`:

```
tail drop: arrived=5 gap=0 dropped=34
```

The daemon read 5 contiguous frames and hung up. The receiver computes
`max(sequence)+1-count = 4+1-5 = 0`, i.e. "no loss", while 34 frames were dropped.
`max(sequence)+1-count` only reveals a hole **followed by a later frame**. This is
why 0-C §8 point 7 requires a client-side DropReport before shipping, and why
`li-hook` reports both `dropped` and `receiver_gap` instead of one number.

## 13-case matrix: what was and was not measured

9 of 13 cases run end-to-end on all three installed shells (bash 5.3.20, zsh 5.9.2,
fish 4.9.3 — all present on this host). Four are reported `NOT MEASURED`:

| case | why not |
|---|---|
| t02 backslash continuation | needs a multi-line pty driver, not a command list |
| t08 background job | 0-A: completion is never observed, by design — nothing new to assert |
| t11 shell SIGKILLed | needs an external killer and a pty transcript |
| t12 20 000-line output | a throughput measure, not a hook property |
