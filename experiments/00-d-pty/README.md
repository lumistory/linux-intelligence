# 0-D — PTY prototype and cross-shell compatibility

Status: README written before any measurement (protocol §0.2).

## Objective

Verify that a supervisor can create its **own** pseudo-terminal, start an
interactive shell inside it, type into it, read the result back, resize it, and
run several of them concurrently without cross-talk — for `bash`, `zsh`
(local 5.9.2 copy, not installed), and `fish`.

Also establish, with evidence, the hard boundary of the mechanism: a PTY lets
you own a **new** terminal session; it does **not** let you attach to a terminal
that already exists and belongs to another program.

## Hypothesis

H-D1: `posix_openpt`/`openpty` + `setsid` + `TIOCSCTTY` + `dup2` start of a
shell gives full bidirectional control; command execution can be confirmed by
sending a sentinel command whose output contains a unique token.

H-D2: prompts and echo differ per shell and must be handled per shell: bash
honours `PS1` from the environment in non-login interactive mode, zsh does too
(`-f` to skip rc), whereas **fish** overwrites the prompt from its own
`fish_prompt` function and shows a `⏎`-style continuation, so matching a literal
prompt string is not portable — the robust signal is the sentinel output plus a
per-shell "ready" marker injected through a real command.

H-D3: full-screen programs (`vi`, `less`) drive the terminal through
alternate-screen and cursor-motion escape sequences; a naive line-scan of the
master stream cannot reconstruct their content, and only the crash-safe facts
(process alive, exit code, terminal mode) are reliable. `top -b -n1` (batch,
not screen-oriented) is the control case and produces plain lines.

H-D4: `TIOCSWINSZ` on the master raises `SIGWINCH` in the foreground process
group automatically (kernel does it on window-size change), so `stty size`
run afterwards reports the new geometry without any manual `kill -WINCH`.

H-D5: two PTYs whose masters are both held by the same supervisor process
cannot interfere: each has its own `pts/N`, its own session id, and its own
line discipline. Concurrency is a matter of per-session state, not kernel
contention.

## Method

- Rust prototype `li-pty-proto` (deps: `nix`, `libc`, `serde_json`, `thiserror`),
  `--offline`.
- One PTY per session: `openpty` → `fork` → in child `setsid()`,
  `TIOCSCTTY`, `dup2(slave, 0/1/2)`, `execve` the shell with a minimal,
  isolated environment.
- **Isolation rule**: no rc file of the host is ever read. bash is started with
  `--norc --noprofile`, zsh with `-f`, fish with `--no-config`. `HOME` points
  into `/tmp/li-phase0-pty-*`. Nothing is written to `~/.bashrc`,
  `~/.zshrc`, `~/.config/fish/*`.
- Master side is `O_NONBLOCK` + `poll`, output captured raw (with a `cat -v`
  variant kept for the report so escape sequences stay legible).
- Window size changes are applied with `TIOCSWINSZ` on the master.
- Concurrency: N sessions in one process, each with its own sentinel token;
  the test asserts every token appears only on its own session's stream.

### Test matrix

| test | bash | zsh | fish |
|---|---|---|---|
| start + run sentinel command | required | required | required |
| echo of typed line observed | required | required | required |
| command substitution / pipe | required | required | required |
| `^C` (0x03) during a `sleep` | required | required | required |
| `^D` (0x04) → shell exits | required | required | required |
| `TIOCSWINSZ` → `stty size` | required | required | required |
| `vi` full-screen + quit | required | required | required |
| `less` full-screen + quit | required | required | required |
| `top -b -n1` control case | required | required | required |
| 4 concurrent sessions, no cross-talk | required | required | required |
| `zsh` prompt handling vs. literal match | — | required | — |

## Out of scope

- terminfo-driven screen rendering / a real terminal emulator (only escape
  sequence *presence* is recorded, plus exit codes).
- Attaching to a terminal owned by another process. This is not a limitation to
  be worked around: it is the mechanism boundary, demonstrated in REPORT §5 by
  `TIOCSTI` being disabled (`dev.tty.legacy_tiocsti=0`) and by `/proc/<pid>/fd/0`
  showing a foreign `pts/N`.
- Shell integration hooks (0-A), IPC transport (0-C).
