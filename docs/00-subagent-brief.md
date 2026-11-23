# Common brief for Phase 0 experiment workers

You are working inside `<WORKSPACE>`, a from-scratch research
project ("linux-intelligence": Linux System Observability & Operation Recovery
Infrastructure). Phase 0 is **pure verification**, not product code.

## Non-negotiable rules

1. **Never fabricate a result.** If a test did not run, write `NOT TESTED`.
   If a hypothesis failed, record the failure. A negative result is a valid
   deliverable; an invented positive result is a project-killing defect.
2. **No destructive commands.** Never `rm -rf`, never touch paths outside your
   experiment directory, `/tmp/li-phase0-<exp>`, and your own cargo target dir.
   Never run anything as root, never `sudo`. Never modify the host shell rc files
   (`~/.bashrc`, `~/.zshrc`, `~/.config/fish/*`). Use isolated configs only.
3. **Everything reproducible.** Every claim in your report must be backed by a
   file under `experiments/<exp>/results/` containing the raw command output,
   and by a script under `experiments/<exp>/` that regenerates it.
4. **Rust**: stable toolchain only (1.99 installed), `cargo fmt`, `cargo clippy
   -- -D warnings` clean, no `unsafe` unless unavoidable (then justify it),
   explicit error types (`thiserror` or hand-written), no panics on the happy path.
   Prefer few dependencies; `rusqlite`, `tokio`, `nix`, `serde`, `inotify`,
   `uuid` are allowed. Check `cargo` offline availability: the crate cache may be
   warm from other projects — if a network fetch fails, say so instead of
   pretending the crate is unavailable.
5. Read `docs/00-phase0-protocol.md` first. Your `REPORT.md` must use its exact
   8-section format.
6. You cannot verify what you did not execute. Print the actual command output
   into the results file with `2>&1 | tee`.

## Deliverables per experiment

```
experiments/<exp>/
  README.md      # objective, hypothesis, method  (write FIRST)
  run.sh         # deterministic driver, set -euo pipefail, exits non-zero on failure
  src/           # prototype code (Rust crate or shell scripts)
  results/       # raw logs, timestamped filenames
  REPORT.md      # the 8 sections
```

## Environment facts (already audited, do not re-audit)

- Host: Arch Linux, kernel 7.2.8-zen1-2-zen, x86_64, btrfs root, systemd 262,
  rustc/cargo 1.99.0, sqlite3 3.53.4, git 2.56, snapper 0.13.2, btrfs-progs 7.1,
  20 cores, 15 GB RAM, nofile 1024, inotify max_user_watches 524288,
  max_queued_events 16384, rootless podman 6.1.3 available.
- `bash` 5.3.20, `fish` 4.9.3 installed. `zsh` is NOT installed system-wide;
  a local copy is unpacked at `experiments/toolchain/root/usr/bin/zsh`
  (zsh 5.9.2). `nu` (nushell) is not installed.
- No sudo password available: any privileged test must use rootless user
  namespaces (`unshare -Ur`) or podman.

## Reporting style

Reports are engineering documents in English. Be blunt about limits.
Table-driven summaries where possible. No marketing language.
