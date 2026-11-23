# 0-F — Cross-distribution compatibility verification

Status: written before any measurement was taken (protocol §0.2).

## Objective

Determine empirically which properties of a Linux host a Phase 1 *provider*
may rely on, and which are distribution-specific. Concretely: can a single
capability probe (a POSIX `sh` script + a tiny syscall helper) discover
everything the product needs across Debian/Ubuntu (dash world) and
Fedora/Arch (bash world) — so that Phase 1 selects a provider by **measured
capability** instead of by distribution name?

Secondary objective: establish the binary distribution policy (dynamic vs
static vs minimum glibc baseline) with real error messages, not folklore.

## Hypothesis

H1. `/etc/os-release` alone is insufficient: four mainstream distributions
    disagree on init, on `/bin/sh`, on available recovery tooling, and on the
    glibc ABI, so `ID=` cannot drive provider selection.
H2. The *kernel-facing* facts we need (procfs layout, inotify limits, pid_max,
    ability to create watches and bind sockets) are identical across all four,
    because they come from the shared host kernel, not the userland.
H3. `systemctl` binary presence is a false positive signal for init; the only
    reliable test is `/proc/1/comm == systemd`.
H4. A dynamic binary linked against the build host's glibc will fail to start
    on older-glibc distributions with a `GLIBC_x.y not found` loader error;
    a fully static binary will start everywhere.
H5. `/bin/sh` differing between dash and bash changes behavior of a
    "portable" script in observable ways (`$BASH_VERSION`, `echo -e`,
    `source`, `[[ ]]`), so any probe script must be strict POSIX.

## Method

1. Four rootless podman containers (no `--privileged`, no sudo, network not
   used — images are already local):
   `archlinux:latest`, `ubuntu:24.04`, `debian:13`, `fedora:42`.
2. Per distribution, capture into separate files under `results/`:
   - identification: `os-release` fields, package manager presence,
     `/bin/sh` target, `/proc/1/comm`
   - shell differences: bash version, `/bin/sh` implementation, `zsh`/`fish`
     presence, plus executed behavioral differences
   - procfs/kernel: `/proc/self/stat` field count, `pid_max`, inotify limits,
     `uname -r`
   - container capabilities: create an inotify watch, bind a unix socket in
     `/tmp`, `ls /proc`; failures recorded verbatim (seccomp)
   - glibc: `ldd --version | head -1`
   - recovery tooling: `btrfs`, `snapper`, `lvm`, `git`
   - init detection: `systemctl` presence vs `/proc/1/comm`
3. `probe.sh` — strict POSIX `sh`, emits one JSON object — run in all four
   containers and on the host. The host run is repeated under `bash` and under
   the container `dash` to prove the script is shell-agnostic.
4. Distribution test: compile three binaries on the host — Rust dynamic,
   Rust fully static, and a C static helper — copy them into each container
   and execute. Record exact loader errors.

Every step has `timeout` protection; every container invocation uses `--rm`.
`run.sh` is idempotent and can be re-run at any time.

## Expected outcome (written before the run)

H1 supported (four-way disagreement), H2 supported (kernel facts equal),
H3 supported (containers have no systemd as PID 1), H4 to be measured —
Arch glibc is expected to be newer than Ubuntu 24.04's and therefore to break
dynamic binaries, H5 supported.

If H2 fails, the "capability probe" approach loses much of its value and the
report must say so.

## Non-goals

No package installation inside containers (network is slow and unnecessary).
No claim that container behavior equals a real install — see REPORT.md §6.
