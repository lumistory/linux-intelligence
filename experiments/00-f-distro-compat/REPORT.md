# 0-F — Cross-distribution compatibility

## 1. Objective

Establish, by measurement, which host properties a Phase 1 provider may rely
on across mainstream Linux distributions, and which are distribution-specific.
Two questions have to be answered with data:

1. **Can a capability probe replace distribution detection?** If the facts the
   product needs (procfs layout, inotify limits, init identity, shell identity,
   libc ABI, recovery-tool availability) can be read at runtime from a portable
   POSIX `sh` script, then Phase 1 can select a provider by *measured
   capability* instead of by `/etc/os-release` `ID=`.
2. **How must the binary be distributed?** Dynamic against a glibc baseline,
   fully static, or per-distribution builds?

Four distributions were chosen to cover both shell worlds and both package
manager families: Arch (rolling, bash-as-sh, pacman), Fedora (bash-as-sh, dnf),
Debian 13 and Ubuntu 24.04 (dash-as-sh, apt).

## 2. Hypothesis

| # | Hypothesis | Predicted |
|---|---|---|
| H1 | `/etc/os-release` alone is insufficient; the four disagree on init detection, `/bin/sh`, recovery tooling, and glibc ABI | supported |
| H2 | Kernel-facing facts (procfs field count, `pid_max`, inotify limits, watch/socket creation) are identical across all four, because all four share the host kernel | supported |
| H3 | `systemctl` binary presence is a false signal for init; only `/proc/1/comm == systemd` is sound | supported |
| H4 | A dynamic binary built on Arch fails on older-glibc distributions with a real loader error; a static binary runs everywhere | to be measured |
| H5 | dash vs bash changes the behavior of a "portable" script observably (`$BASH_VERSION`, `echo -e`, `[[ ]]`, `source`) | supported |

## 3. Method

Environment: Arch Linux host, kernel `7.2.8-zen1-2-zen`, x86_64, rootless
podman 6.1.3, **no sudo, no `--privileged`**, network disabled for every
container run (`--network none`), no image pulls (all four images were already
local). Every container invocation is `timeout 120 podman run --rm`.

Artefacts under test:

| file | what it is |
|---|---|
| `probe.sh` | strict POSIX `sh` capability probe; emits one JSON object; no bashisms |
| `src/capcheck.c` | fully static C helper; performs the real syscalls a POSIX shell cannot (inotify, unix socket, netlink, fork) |
| `src/probes/p1..p9_*.sh` | per-step POSIX probes, one per measured area |
| `results/bin/hello_dyn` | Rust, dynamically linked against Arch glibc 2.44 |
| `results/bin/hello_static` | Rust, fully static (`-C target-feature=+crt-static`) |
| `results/bin/hello_newer` | C, deliberately requires `GLIBC_2.43` via `memset_explicit` |

`run.sh` orchestrates 8 stages and writes one file per step into `results/`.
It is idempotent and was run end-to-end repeatedly; the final full run exited 0.
The console only ever shows the last 20 lines of a step; full output lives in
`results/`.

Cross-check on the probe itself: `probe.sh` was executed on the host under
**bash** and under **dash** (extracted from `debian:13`, because Arch ships no
dash). Both runs produced byte-identical JSON except for the shell's own
identity fields and two live kernel counters — see §4.

## 4. Raw results

### 4.1 Per-distribution × per-probe matrix

Every cell is a measured value with the file that contains the raw evidence.
`results/30_matrix.log` is the machine-generated version of the top half.

| Probe item | Arch (rolling) | Ubuntu 24.04 | Debian 13 | Fedora 42 |
|---|---|---|---|---|
| **`os-release` ID** | `arch`<br>`10_ident_arch.log` | `ubuntu`<br>`10_ident_ubuntu2404.log` | `debian`<br>`10_ident_debian13.log` | `fedora`<br>`10_ident_fedora42.log` |
| **`ID_LIKE`** | *(empty)*<br>`10_ident_arch.log` | `debian`<br>`10_ident_ubuntu2404.log` | *(empty)*<br>`10_ident_debian13.log` | *(empty)*<br>`10_ident_fedora42.log` |
| **`VERSION_ID`** | `20261108.0.623074`<br>`10_ident_arch.log` | `24.04`<br>`10_ident_ubuntu2404.log` | `13`<br>`10_ident_debian13.log` | `42`<br>`10_ident_fedora42.log` |
| **package manager** | `pacman` `/usr/sbin/pacman`<br>`10_ident_arch.log` | `apt` `/usr/bin/apt`<br>`10_ident_ubuntu2404.log` | `apt` `/usr/bin/apt`<br>`10_ident_debian13.log` | `dnf` `/usr/sbin/dnf`<br>`10_ident_fedora42.log` |
| **`ls -l /bin/sh`** | `-> bash`<br>`10_ident_arch.log` | `-> dash`<br>`10_ident_ubuntu2404.log` | `-> dash`<br>`10_ident_debian13.log` | `-> bash`<br>`10_ident_fedora42.log` |
| **`/proc/1/comm`** | `sh`<br>`10_ident_arch.log` | `sh`<br>`10_ident_ubuntu2404.log` | `sh`<br>`10_ident_debian13.log` | `sh`<br>`10_ident_fedora42.log` |
| **bash version** | `5.3.20(1)-release`<br>`11_shell_arch.log` | `5.2.21(1)-release`<br>`11_shell_ubuntu2404.log` | `5.2.37(1)-release`<br>`11_shell_debian13.log` | `5.2.37(1)-release`<br>`11_shell_fedora42.log` |
| **`$BASH_VERSION` under `/bin/sh`** | SET<br>`11_shell_arch.log` | **EMPTY**<br>`11_shell_ubuntu2404.log` | **EMPTY**<br>`11_shell_debian13.log` | SET<br>`11_shell_fedora42.log` |
| **`local` under `/bin/sh`** | accepted<br>`11_shell_arch.log` | accepted<br>`11_shell_ubuntu2404.log` | accepted<br>`11_shell_debian13.log` | accepted<br>`11_shell_fedora42.log` |
| **`echo -e` (od -c arbiter)** | `a \t b` — TAB expanded, `-e` consumed<br>`11_shell_arch.log` | `- e   a \t b` — `-e` printed as data<br>`11_shell_ubuntu2404.log` | `- e   a \t b` — `-e` printed as data<br>`11_shell_debian13.log` | `a \t b` — TAB expanded, `-e` consumed<br>`11_shell_fedora42.log` |
| **`echo` without `-e`** | literal `\ t` kept (5 bytes)<br>`11_shell_arch.log` | TAB expanded (4 bytes)<br>`11_shell_ubuntu2404.log` | TAB expanded (4 bytes)<br>`11_shell_debian13.log` | literal `\ t` kept (5 bytes)<br>`11_shell_fedora42.log` |
| **`printf`** | `a \t b` (4 bytes)<br>`11_shell_arch.log` | `a \t b` (4 bytes)<br>`11_shell_ubuntu2404.log` | `a \t b` (4 bytes)<br>`11_shell_debian13.log` | `a \t b` (4 bytes)<br>`11_shell_fedora42.log` |
| **`[[ ]]` under `/bin/sh`** | ACCEPTED<br>`11_shell_arch.log` | REJECTED<br>`11_shell_ubuntu2404.log` | REJECTED<br>`11_shell_debian13.log` | ACCEPTED<br>`11_shell_fedora42.log` |
| **`source` under `/bin/sh`** | ACCEPTED<br>`11_shell_arch.log` | REJECTED<br>`11_shell_ubuntu2404.log` | REJECTED<br>`11_shell_debian13.log` | ACCEPTED<br>`11_shell_fedora42.log` |
| **`set -o pipefail`** | SUPPORTED<br>`18_shtrap_arch.log` | **ABORTS** (`Illegal option -o pipefail`, dash 0.5.12-6ubuntu5)<br>`18_shtrap_ubuntu2404.log` | SUPPORTED (dash 0.5.12-12)<br>`18_shtrap_debian13.log` | SUPPORTED<br>`18_shtrap_fedora42.log` |
| **zsh / fish present** | no / no<br>`11_shell_arch.log` | no / no<br>`11_shell_ubuntu2404.log` | no / no<br>`11_shell_debian13.log` | no / no<br>`11_shell_fedora42.log` |
| **`/proc/self/stat` fields** | `52`<br>`12_procfs_arch.log` | `52`<br>`12_procfs_ubuntu2404.log` | `52`<br>`12_procfs_debian13.log` | `52`<br>`12_procfs_fedora42.log` |
| **`pid_max`** | `4194304`<br>`12_procfs_arch.log` | `4194304`<br>`12_procfs_ubuntu2404.log` | `4194304`<br>`12_procfs_debian13.log` | `4194304`<br>`12_procfs_fedora42.log` |
| **inotify `max_user_watches`** | `524288`<br>`12_procfs_arch.log` | `524288`<br>`12_procfs_ubuntu2404.log` | `524288`<br>`12_procfs_debian13.log` | `524288`<br>`12_procfs_fedora42.log` |
| **inotify `max_user_instances`** | `1024`<br>`12_procfs_arch.log` | `1024`<br>`12_procfs_ubuntu2404.log` | `1024`<br>`12_procfs_debian13.log` | `1024`<br>`12_procfs_fedora42.log` |
| **inotify `max_queued_events`** | `16384`<br>`12_procfs_arch.log` | `16384`<br>`12_procfs_ubuntu2404.log` | `16384`<br>`12_procfs_debian13.log` | `16384`<br>`12_procfs_fedora42.log` |
| **`uname -r`** | `7.2.8-zen1-2-zen` (host kernel)<br>`12_procfs_arch.log` | `7.2.8-zen1-2-zen` (host kernel)<br>`12_procfs_ubuntu2404.log` | `7.2.8-zen1-2-zen` (host kernel)<br>`12_procfs_debian13.log` | `7.2.8-zen1-2-zen` (host kernel)<br>`12_procfs_fedora42.log` |
| **create inotify watch** | ok (`wd=1`)<br>`13_capability_arch.log` | ok (`wd=1`)<br>`13_capability_ubuntu2404.log` | ok (`wd=1`)<br>`13_capability_debian13.log` | ok (`wd=1`)<br>`13_capability_fedora42.log` |
| **bind unix socket in `/tmp`** | ok (bind+listen+connect)<br>`13_capability_arch.log` | ok<br>`13_capability_ubuntu2404.log` | ok<br>`13_capability_debian13.log` | ok<br>`13_capability_fedora42.log` |
| **`ls /proc`** | ok (2 numeric entries, `capcheck`); probe.sh saw 4<br>`13_capability_arch.log`, `20_probe_arch.json` | ok (2 / 4)<br>`13_capability_ubuntu2404.log` | ok (2 / 4)<br>`13_capability_debian13.log` | ok (2 / 4)<br>`13_capability_fedora42.log` |
| **netlink socket / fork** | ok / ok<br>`13_capability_arch.log` | ok / ok<br>`13_capability_ubuntu2404.log` | ok / ok<br>`13_capability_debian13.log` | ok / ok<br>`13_capability_fedora42.log` |
| **`Seccomp:` / `CapEff`** | `2` (filter mode) / `00000000800405fb`<br>`13_capability_arch.log` | `2` / `00000000800405fb`<br>`13_capability_ubuntu2404.log` | `2` / `00000000800405fb`<br>`13_capability_debian13.log` | `2` / `00000000800405fb`<br>`13_capability_fedora42.log` |
| **`ldd --version`** | `ldd (GNU libc) 2.44`<br>`14_glibc_arch.log` | `ldd (Ubuntu GLIBC 2.39-0ubuntu8.9) 2.39`<br>`14_glibc_ubuntu2404.log` | `ldd (Debian GLIBC 2.41-12+deb13u4) 2.41`<br>`14_glibc_debian13.log` | `ldd (GNU libc) 2.41`<br>`14_glibc_fedora42.log` |
| **glibc** | **2.44** | **2.39** | **2.41** | **2.41** |
| **musl loader** | absent<br>`14_glibc_arch.log` | absent<br>`14_glibc_ubuntu2404.log` | absent<br>`14_glibc_debian13.log` | absent<br>`14_glibc_fedora42.log` |
| **btrfs-progs** | ABSENT<br>`15_recovery_arch.log` | ABSENT<br>`15_recovery_ubuntu2404.log` | ABSENT<br>`15_recovery_debian13.log` | ABSENT<br>`15_recovery_fedora42.log` |
| **snapper** | ABSENT<br>`15_recovery_arch.log` | ABSENT<br>`15_recovery_ubuntu2404.log` | ABSENT<br>`15_recovery_debian13.log` | ABSENT<br>`15_recovery_fedora42.log` |
| **lvm2** | ABSENT<br>`15_recovery_arch.log` | ABSENT<br>`15_recovery_ubuntu2404.log` | ABSENT<br>`15_recovery_debian13.log` | ABSENT<br>`15_recovery_fedora42.log` |
| **git** | ABSENT<br>`15_recovery_arch.log` | ABSENT<br>`15_recovery_ubuntu2404.log` | ABSENT<br>`15_recovery_debian13.log` | ABSENT<br>`15_recovery_fedora42.log` |
| **`systemctl` binary** | **PRESENT** `/usr/bin/systemctl`<br>`16_initdetect_arch.log` | ABSENT<br>`16_initdetect_ubuntu2404.log` | ABSENT<br>`16_initdetect_debian13.log` | ABSENT<br>`16_initdetect_fedora42.log` |
| **`systemctl is-system-running`** | `offline`<br>`16_initdetect_arch.log` | n/a (no binary)<br>`16_initdetect_ubuntu2404.log` | n/a<br>`16_initdetect_debian13.log` | n/a<br>`16_initdetect_fedora42.log` |
| **`/run/systemd/system`** | ABSENT<br>`16_initdetect_arch.log` | ABSENT<br>`16_initdetect_ubuntu2404.log` | ABSENT<br>`16_initdetect_debian13.log` | ABSENT<br>`16_initdetect_fedora42.log` |
| **systemd actually init?** | **no** (PID 1 = `sh`)<br>`16_initdetect_arch.log` | no<br>`16_initdetect_ubuntu2404.log` | no<br>`16_initdetect_debian13.log` | no<br>`16_initdetect_fedora42.log` |
| **unit dirs exist anyway?** | yes (`/usr/lib/systemd/system`)<br>`16_initdetect_arch.log` | yes<br>`16_initdetect_ubuntu2404.log` | yes<br>`16_initdetect_debian13.log` | yes<br>`16_initdetect_fedora42.log` |
| **`hello_dyn` (Arch glibc 2.44 build)** | **runs** (exit 0)<br>`17_distbins_arch.log` | **runs** (exit 0)<br>`17_distbins_ubuntu2404.log` | **runs** (exit 0)<br>`17_distbins_debian13.log` | **runs** (exit 0)<br>`17_distbins_fedora42.log` |
| **`hello_static`** | runs (0)<br>`17_distbins_arch.log` | runs (0)<br>`17_distbins_ubuntu2404.log` | runs (0)<br>`17_distbins_debian13.log` | runs (0)<br>`17_distbins_fedora42.log` |
| **`hello_newer` (needs GLIBC_2.43)** | **runs** (0)<br>`17_distbins_arch.log` | **FAILS** (1)<br>`17_distbins_ubuntu2404.log` | **FAILS** (1)<br>`17_distbins_debian13.log` | **FAILS** (1)<br>`17_distbins_fedora42.log` |
| **`read < /proc/...` returns whole value?** | yes (`4194304`)<br>`18_shtrap_arch.log` | **NO — truncated to `4`**<br>`18_shtrap_ubuntu2404.log` | **NO — truncated to `4`**<br>`18_shtrap_debian13.log` | yes (`4194304`)<br>`18_shtrap_fedora42.log` |
| **`stat -c %s` on that procfs file** | `0`<br>`18_shtrap_arch.log` | `0`<br>`18_shtrap_ubuntu2404.log` | `0`<br>`18_shtrap_debian13.log` | `0`<br>`18_shtrap_fedora42.log` |
| **cgroup version / `ulimit -n`** | v2 / `524288`<br>`12_procfs_arch.log` | v2 / `524288`<br>`12_procfs_ubuntu2404.log` | v2 / `524288`<br>`12_procfs_debian13.log` | v2 / `524288`<br>`12_procfs_fedora42.log` |
| **`ulimit -u`** | `61799`<br>`12_procfs_arch.log` | `unavailable` (dash: `Illegal option -u`)<br>`12_procfs_ubuntu2404.log` | `unavailable`<br>`12_procfs_debian13.log` | `61799`<br>`12_procfs_fedora42.log` |

### 4.2 Host baseline (the same `probe.sh`, outside any container)

| Field | Host (bash) | Host (dash, extracted from debian:13) | Container (arch) |
|---|---|---|---|
| `pid1_comm` | `systemd` | `systemd` | `sh` |
| `pid1_is_systemd` | `true` | `true` | `false` |
| `systemctl_present` | `true` | `true` | `true` |
| `is-system-running` | `running` | `running` | `offline` |
| `bin_sh_impl` | `bash` | `bash` | `bash` |
| `glibc_version` | `2.44` | `2.44` | `2.44` |
| `self_stat_fields` | `52` | `52` | `52` |
| `pid_max` | `4194304` | `4194304` | `4194304` |
| `max_user_watches` | `524288` | `524288` | `524288` |
| `max_user_instances` | `1024` | `1024` | `1024` |
| `max_queued_events` | `16384` | `16384` | `16384` |
| `btrfs` / `snapper` / `git` | `/usr/bin/btrfs` / `/usr/bin/snapper` / `/usr/bin/git` | same | ABSENT / ABSENT / ABSENT |

Evidence: `01_probe_host_bash.json`, `01_probe_host_dash.json`,
`01_probe_host_bash.log`, `01_probe_host_dash.log`, `01_host_capcheck.log`,
`20_probe_arch.json`.

**Probe portability proof.** `diff` of the two host JSONs, after removing the
shell-identity fields, differs only in `ns_last_pid` and
`visible_numeric_proc_entries` — both live counters that change between any two
process launches. Every structural field is byte-identical under bash and under
dash. This is the operational proof that `probe.sh` is shell-agnostic.

### 4.3 Distribution test, verbatim loader errors

`results/31_dist_matrix.log`, from `results/17_distbins_*.log`:

```
hello_newer: /lib64/libc.so.6: version `GLIBC_2.43' not found (required by hello_newer)
hello_newer: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.43' not found (required by hello_newer)
```

Exit codes measured, `0` = ran, `1` = failed:

| distro | container glibc | `hello_dyn` | `hello_static` | `hello_newer` |
|---|---|---|---|---|
| arch | 2.44 | 0 | 0 | 0 |
| ubuntu2404 | 2.39 | **0** | 0 | **1** |
| debian13 | 2.41 | **0** | 0 | **1** |
| fedora42 | 2.41 | **0** | 0 | **1** |

Version nodes required by each artefact (host-side `objdump -T`,
`results/00_build_artefacts.log`):

| artefact | highest GLIBC node required |
|---|---|
| `hello_dyn` (Rust, dynamic) | `GLIBC_2.34` |
| `hello_static` (Rust, static) | *(none — static-pie)* |
| `hello_newer` (C, `memset_explicit`) | `GLIBC_2.43` |
| `capcheck` (C, static) | *(none)* |
| `dash` 0.5.12-12 (extracted) | `GLIBC_2.38` |

### 4.4 The dash procfs trap

`results/34_shtrap_matrix.log`, from `results/18_shtrap_*.log`:

| distro | `/bin/sh` | `read -r v </proc/sys/kernel/pid_max` | `v=$(cat ...)` | `stat -c %s` |
|---|---|---|---|---|
| arch | bash | `4194304` | `4194304` | `0` |
| ubuntu2404 | dash 0.5.12-6ubuntu5 | **`4`** | `4194304` | `0` |
| debian13 | dash 0.5.12-12 | **`4`** | `4194304` | `0` |
| fedora42 | bash | `4194304` | `4194304` | `0` |

`probe.sh` was initially written with `read -r line < "$file"` and produced
`null` for `pid_max` and all three inotify limits inside Ubuntu and Debian.
That was a **real defect in the probe, found by the experiment**, not a
container property. Fixed by using `cat | sed -n 1p` (see §5.4).

### 4.5 pipefail divergence inside dash

| distro | dash package version | `set -o pipefail` |
|---|---|---|
| ubuntu2404 | `0.5.12-6ubuntu5` | rejects: `Illegal option -o pipefail` (exit 2) |
| debian13 | `0.5.12-12` | accepts; `false \| true` yields exit **1** |

Two dash patchlevels, two behaviours. `pipefail` is not in POSIX and is not
portable across `sh` implementations even within one distro family.

## 5. Analysis

### 5.1 H1 — os-release is not enough (supported)

The four distributions disagree on **nine** of the twenty-one probed fields,
and every disagreement is a field that matters:

| field | consequence if you trust it |
|---|---|
| `ID` / `VERSION_ID` / `ID_LIKE` | Arch has **no `VERSION_ID` on the host** but **does** inside the container image — the same "distribution" reports different values. Nothing can be keyed on version strings. |
| `bin_sh_impl` | dash vs bash changes `$BASH_VERSION`, `echo -e`, `[[ ]]`, `source`, `pipefail`. |
| `systemctl_present` | Arch reports PRESENT while systemd is **not running**. |
| `glibc_version` | 2.39 / 2.41 / 2.41 / 2.44 — a dynamic binary's viability depends on it. |

The full measured split, from `results/30_matrix.log` (12 fields SAME,
9 DIFFERS):

| SAME across all four | DIFFERS across the four |
|---|---|
| `init.pid1_comm`, `init.pid1_is_systemd`, `init.run_systemd_dir` | `init.systemctl_present` |
| `procfs.self_stat_fields`, `procfs.pid_max` | `os_release.ID`, `os_release.ID_LIKE`, `os_release.VERSION_ID`, `os_release.pkg_manager` |
| `inotify.max_user_watches`, `max_user_instances`, `max_queued_events` | `shells.bin_sh_impl`, `shells.bash_version`, `probe_under_bash` |
| `recovery_tools.btrfs`, `.snapper`, `.lvm`, `.git` (all absent in all four) | `libc.glibc_version` |

Note the exception: the four `recovery_tools.*` fields are SAME, but *not*
because they are kernel facts — they are SAME because all four **minimal
container images** happen to omit every recovery tool. That is an artefact of
the images, not a cross-distribution property, and §6/L6 warns against reading
it as one. Every *other* SAME field is genuinely kernel-derived; every DIFFERS
field is userland-derived. That split is the whole result of this experiment.

The strongest single argument against name-based branching: the host and the
Arch container are the *same distribution* yet differ in `VERSION_ID`
presence, in recovery tooling, and in init. `ID=arch` cannot tell Phase 1
which of those two situations it is in.

### 5.2 H2 — kernel facts are identical (supported, with one caveat)

All kernel-derived numbers match exactly across the four containers **and** the
host: `uname -r` = `7.2.8-zen1-2-zen`, `/proc/self/stat` = 52 fields,
`pid_max` = 4194304, inotify limits 524288 / 1024 / 16384.

This is the central positive result: **the kernel surface the product depends
on is distribution-independent.** Nothing in Phase 1's procfs/inotify
observation layer needs a per-distro code path.

The caveat is that this equality is a consequence of the containers sharing the
host kernel, not evidence about a stock Debian/Fedora kernel. A stock kernel may
ship different `pid_max` defaults or inotify maxima. The *stability* claim
holds; the *specific values* do not transfer.

One kernel-facing field *does* differ, and it is the one worth flagging:
`/proc` is **PID-namespaced**. The host exposes 619 numeric `/proc` entries; each
container exposes 4. So "the kernel surface is identical" is true for the
*tunable* readings and false for *process visibility*. A provider must never
infer "this machine has N processes" from `ls /proc` without knowing which PID
namespace it is in — `procfs.visible_numeric_proc_entries` is
namespace-relative, unlike `pid_max`.

### 5.3 H3 — systemctl presence is a false signal (supported)

Arch is the decisive case: `systemctl` is **PRESENT** at `/usr/bin/systemctl`,
`systemctl is-system-running` answers **`offline`** (a valid systemd answer!),
`/usr/lib/systemd/system` exists — and yet PID 1 is `sh`. Every
binary-and-directory heuristic says "systemd is here"; the truth is that systemd
is not running.

`/proc/1/comm == systemd` is the only test that gave the correct answer in all
five environments (4 containers say `false`, host says `true`). Note the
heuristic `/run/systemd/system` also worked here, but only because the container
runtime does not create it; it is a side effect, not a definition.

`is-system-running` returning `offline` is especially dangerous: it is a
plausible-looking systemd state string, so a provider that keys on "did
systemctl answer" would conclude systemd exists.

### 5.4 The probe itself was the weakest link (a real finding)

Two defects were found and fixed *because* the experiment ran on four
distributions:

1. **`read < procfile` truncates under dash.** procfs reports `st_size = 0`;
   dash trusts `st_size` and reads one byte. bash reads until EOF. A probe built
   on `read` silently reports `4` instead of `4194304` for `pid_max` on
   Debian/Ubuntu. `cat` is correct under both.
2. **`set -o pipefail` aborts Ubuntu's dash**, and the failure is silent in a
   `2>/dev/null || fallback` construct: the shell exits before the `||` branch
   runs, so the fallback never executes. Every probe script in this experiment
   was audited for this; `probe.sh` does not use `pipefail`, `local`, `[[ ]]`,
   `source`, arrays, or `echo -e`.

Both are the same lesson: **the portability risk is in our probe script, not
only in the distributions.** A capability probe written on bash assumptions
produces confidently wrong capability data on half the fleet.

### 5.5 H5 — shell differences, precisely characterised (supported)

| behaviour | bash-as-`sh` (Arch, Fedora) | dash (Ubuntu, Debian) | portable answer |
|---|---|---|---|
| `$BASH_VERSION` | set | empty | never rely on it; use `$0`/`readlink /proc/$$/exe` |
| `local` | accepted | **accepted** (both dash builds) | technically non-POSIX but works on all four; avoid in strict probes |
| `echo -e "a\tb"` | `a<TAB>b` (4 bytes) | `-e a<TAB>b` (**7 bytes**, `-e` printed) | use `printf` |
| `echo "a\tb"` | literal `\t` kept (5 bytes) | TAB expanded (4 bytes) | use `printf` — POSIX allows both |
| `printf "a\tb\n"` | 4 bytes | 4 bytes | **the only portable option** |
| `[[ ]]`, `source` | accepted | rejected | use `[ ]`, `.` |
| `pipefail` | accepted | Ubuntu: **hard abort**; Debian: accepted | do not use |

`local` is the surprise: it is not POSIX, yet all four accept it. That makes it
"portable in practice" but still a spec violation — `probe.sh` avoids it anyway,
because the next dash or busybox-ash build may not be so forgiving.

`echo` is unusable for portable output: bash keeps `\t` literal, dash expands
it, and POSIX sanctions both. This is why `probe.sh` emits every value through
`printf`.

### 5.6 H4 — glibc distribution (supported, with a nuance)

The naive expectation — "Arch glibc 2.44 will not run on Ubuntu's 2.39" — was
**falsified**. `hello_dyn` ran fine on all four, because Rust's runtime only
pulls version nodes up to `GLIBC_2.34`, and 2.39 > 2.34.

The failure mode is real, just not triggered by *this* binary: `hello_newer`
(needs `GLIBC_2.43`) failed on Ubuntu 2.39, Debian 2.41 and Fedora 2.41 with
the exact loader message recorded in §4.3.

The rule is therefore about *symbols actually referenced*, not about the build
machine's glibc version:

> A dynamically linked binary runs on any distribution whose glibc is at least
> the **highest version node the binary actually references** — not the version
> of the glibc it was compiled against.

Consequences for Phase 1:

- Building on the newest distro is *not* automatically fatal, but it is
  undefended: any future dependency that pulls a newer symbol (e.g. `mseal`,
  `memset_explicit` at 2.43) breaks silently on older targets. The build had to
  be *checked*, not assumed — `objdump -T | grep GLIBC_` is a required CI gate.
- The static binary (`hello_static`, 1.4 MB) ran on all four with zero external
  symbols. That is the only option that requires no baseline reasoning at all.
- glibc floor across a modern fleet: **2.39** (Ubuntu 24.04 LTS, the oldest
  here). Debian 13 and Fedora 42 are both 2.41.

### 5.7 Recovery tooling: the container images are all bare

All four containers lack `btrfs`, `snapper`, `lvm`, `git`, `zfs`, and even the
package managers report them `absent`. This is an artefact of minimal container
images, **not** a statement about real installs — see §6. What it does prove is
that a provider **must** be able to run without them and report their absence
cleanly, and that `dpkg -s` / `pacman -Q` / `rpm -q` can answer "installed?" from
the local package database with no network (`15_recovery_*.log`).

Also note: the containers see a real `btrfs` mount (`/probe`, `/probe-bin` from
the bind mount, `type btrfs`), and `/proc/filesystems` lists `btrfs`, but no
block device is visible (`/dev/nvme*` absent). Filesystem *type* visibility and
filesystem *control* are different capabilities.

### 5.8 Security posture is uniform

All four ran with `Seccomp: 2` (filter mode), `NoNewPrivs: 0`, identical
`CapEff 00000000800405fb`, uid 0 inside the container (rootless uid 1000
outside). No capability probe failed — inotify, unix sockets, netlink, fork all
worked. The default rootless podman profile permits everything Phase 1 needs.

## 6. Known limitations

This experiment ran in containers. **A container is not a distribution
installation.** The following conclusions must not be extrapolated:

| # | Limitation | What it invalidates |
|---|---|---|
| L1 | **No systemd as PID 1.** All four containers run the probe shell as PID 1. | Cannot validate any systemd interaction: unit state, `systemctl` queries, journald, service restart, `is-system-running` semantics on a real system. The Arch container proving that `systemctl` *exists but is not init* is a container artefact — a real Arch install has systemd at PID 1. |
| L2 | **No udev, no device manager.** `/dev` is a minimal tmpfs; no `/dev/nvme*`. | Cannot test device enumeration, hotplug, or `/dev/disk/by-*` resolution. |
| L3 | **Shared host kernel.** All four report `7.2.8-zen1-2-zen`. | The identical `pid_max`, inotify limits, and `/proc/self/stat` = 52 are **kernel** properties of *this host*, not distribution defaults. A stock Debian kernel may differ. The *equality across distros* result stands; the *specific numeric values* do not transfer to other machines. |
| L4 | **seccomp / capability filtering.** `Seccomp: 2`, `CapEff 00000000800405fb`. | Nothing failed here, but a real install has no seccomp filter and has full root capabilities. Absence of a failure in the container does not prove absence of a restriction on real hardware, and vice versa: a syscall blocked here might be permitted there. |
| L5 | **No real disk.** No partition table, no LVM PV, no btrfs subvolume owned by us. | Snapshot creation, rollback, `btrfs subvolume` operations, LVM activation — **NOT TESTED**. This is the single biggest gap for a "recovery" product: the recovery paths themselves were never exercised. |
| L6 | **Minimal images.** None of the four has `git`, `btrfs-progs`, `snapper`, `lvm2`, `zstd`, `file`, `strings`. | The "recovery tooling ABSENT" column says nothing about real installs. On a real Arch box all of these are commonly present; on a minimal server, none. |
| L7 | **Rootless podman, uid 0 in-container.** Inside is uid 0, outside is uid 1000 with a subuid mapping. | Permission behaviour is neither "real root" nor "real unprivileged user". Operations requiring genuine root (mount, `btrfs subvolume`, `lvm`) were never reachable. |
| L8 | **No network.** `--network none` for every run. | Package installation, repository metadata refresh, and any network-dependent recovery (`git fetch`, remote backup) are **NOT TESTED**. |
| L9 | **Ubuntu 24.04's dash is a patched older build.** `0.5.12-6ubuntu5` rejects `pipefail`; Debian's `0.5.12-12` accepts it. | The "dash rejects pipefail" statement is true for *this Ubuntu image*; it is not a stable property of dash. Another Ubuntu point release may change it. |
| L10 | **`hello_dyn` is a trivial program.** It pulls only `GLIBC_2.34` nodes. | It understates the real risk for the Phase 1 binary, whose dependency set is larger. The "dynamic works on 2.39" result applies to *this* binary only; the general rule in §5.6 is what transfers. |
| L11 | **Only 4 distributions, 1 architecture.** x86_64 only. | ARM64, musl-based distros (Alpine), NixOS, and rolling-vs-stable kernel differences are **NOT TESTED**. Alpine in particular has no glibc at all. |
| L12 | **Arch container `VERSION_ID` differs from host.** `20261108.0.623074` vs absent. | Any conclusion keyed on Arch versioning is unsafe; the container image carries a build stamp the installed system does not. |
| L13 | **`ulimit -u` unreadable under dash.** dash has no `-u` option (`Illegal option -u`); only bash-as-`sh` answered (`61799`, Arch and Fedora). | The `nproc` limit is only measurable on bash-backed `/bin/sh`. Use `prlimit` instead in Phase 1. |
| L14 | **Recovery/boot/init flows were never exercised end-to-end.** | 0-F measured *discovery* of state, not *recovery* from a broken state. No conclusion about recovery correctness can be drawn from this experiment. |

## 7. Conclusion

**PASS-WITH-LIMITS.**

Measured and reproducible:

- The kernel-facing surface (procfs layout, `pid_max`, inotify limits, inotify
  watch creation, unix socket bind in `/tmp`, `/proc` traversal) is **identical
  across Arch, Ubuntu 24.04, Debian 13 and Fedora 42**, and identical to the
  host. No distribution-specific code path is needed for it.
- The userland surface disagrees on every field that affects behaviour:
  `/bin/sh` implementation, `echo`/`echo -e` semantics, `[[ ]]`/`source`/
  `pipefail` acceptance, `systemctl` presence and truthfulness, glibc version,
  and recovery-tool availability.
- A strict-POSIX `probe.sh` produced **byte-identical structured output** under
  bash and dash on the host and under four different container userlands. The
  capability-probe approach works.
- Init detection: `/proc/1/comm == systemd` was the only method correct in all
  five environments.
- Binary distribution: the static binary and the dynamic binary both ran on all
  four (the dynamic one only because it stopped at `GLIBC_2.34`); a binary
  referencing `GLIBC_2.43` failed on three of four with a verbatim loader error.

Not established (see §6): anything requiring real systemd, real devices, real
disks, real root, or a network; and any non-x86_64 or musl platform.

Hypotheses: H1, H2, H3, H5 supported. **H4 was partially falsified** — a dynamic
Arch build did *not* fail on older glibc, contradicting the naive version-based
prediction; the real determining factor is the highest referenced symbol
version. That correction is the most valuable result of §5.6.

## 8. Gate

**May Phase 1 implement this? — YES, with the conditions below.**

### Q1. Can capability probing be the Phase 1 provider-selection mechanism?

**Yes.** Evidence: `probe.sh` ran unchanged in all five environments and
returned valid, comparable JSON (`results/20_probe_*.json`, `results/30_matrix.log`).
The kernel fields it reads are identical everywhere, so a provider can trust
them; the userland fields differ, which is exactly why they must be *probed*
rather than *assumed from `ID=`*.

Conditions:

1. **Select on measured facts, never on `ID`/`VERSION_ID`.** `ID=arch` appears on
   both a systemd host and a systemd-less container; `VERSION_ID` is absent on
   the host and present in the image.
2. **`probe.sh` must be strict POSIX** and must not use `local`, `[[ ]]`,
   `source`, arrays, `echo -e`, or `set -o pipefail`. All five were measured to
   differ or to abort (`results/33_shell_matrix.log`, `results/18_shtrap_*.log`).
3. **Never use `read < procfile`.** Use `cat`. dash truncates procfs reads to one
   byte (`results/34_shtrap_matrix.log`).
4. **Init detection = `/proc/1/comm == systemd`,** full stop. Do not use
   `systemctl` presence, `is-system-running` (returns `offline` on a
   non-systemd container), or unit directories.
5. **Capability tests need a static helper.** POSIX `sh` cannot call `inotify_init`
   or `bind`; `capcheck` (static C, no GLIBC nodes) is required and ran on all
   four.
6. **Every capability the probe could not test must be `"unknown"`,** per the
   protocol's capability-record rule. Do not default to `true`.
7. **Capability probing is only as good as the probe.** This experiment's own
   probe had two bugs (§5.4) that produced confidently wrong numbers on 2 of 4
   distros. The probe needs its own test matrix as a permanent CI gate.

### Q2. glibc distribution recommendation

**Ship fully static for the probe/helper binaries; ship dynamic-with-checked-
baseline for the main product, and never assume the build host's glibc is the
floor.**

Measured basis:

| option | measured result | verdict |
|---|---|---|
| Dynamic, built on Arch (2.44) | ran on 2.39/2.41/2.41 — but only because it referenced nothing above `GLIBC_2.34` | works, but undefended |
| Dynamic, referencing `GLIBC_2.43` | **failed on Ubuntu 2.39, Debian 2.41, Fedora 2.41** with `version 'GLIBC_2.43' not found` | proves the failure mode |
| Fully static (`+crt-static`, 1.4 MB) | ran on all four, zero version nodes | **safest** |

Concrete policy:

1. **Static binary** for `capcheck` and any helper that must run inside an
   unknown userland. Size is irrelevant for a one-shot probe.
2. **Minimum baseline: glibc 2.39** (Ubuntu 24.04 LTS, the oldest of the four).
   Build in a 2.39 container, not on the Arch host.
3. **CI gate, mandatory:** `objdump -T $BIN | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1`
   must be `<=` the baseline. A new dependency silently raising the node is the
   exact failure recorded in §4.3.
4. **Do not distribute a purely dynamic binary without that gate.** The
   `hello_dyn` success here was luck, not design.

### Q3. Which conclusions hold only inside containers?

These **must not** be carried into Phase 1 as facts about real systems:

- "systemd is not running on Arch" — true for the *container image* only. A real
  Arch install runs systemd as PID 1. Everything about init detection's *method*
  transfers; the *result* does not.
- "No recovery tooling is installed" — true of all four minimal images; says
  nothing about real installs.
- "`pid_max` = 4194304 and inotify watches = 524288 everywhere" — these are this
  host kernel's values, inherited by every container. Other machines differ.
- "Root can create inotify watches and bind sockets without restriction" — true
  under the default rootless podman seccomp profile; a hardened real system
  (SELinux enforcing, `fs.inotify.max_user_instances` tuned down) may not allow
  it.
- "A dynamic binary just works" — true for a binary needing only `GLIBC_2.34`;
  false in general.
- "No real disk / LVM / btrfs subvolume work" — **NOT TESTED** at all, so nothing
  at all can be concluded about snapshot or rollback capability.

What **does** transfer, because it was measured on the shared kernel and is
shell-level rather than container-level:

- `/proc/self/stat` has 52 fields on kernel 7.2.x.
- dash vs bash behavioural differences (§5.5) — these are userland, and the
  container images carry genuine Debian/Ubuntu dash and Fedora/Arch bash.
- The glibc ABI failure mode and its exact error message.
- The validity of `/proc/1/comm` as the init test.

> **路径替换说明**：本次公开前对 `results/` 与脚本里的绝对路径做了字符串替换，换成两个占位符 `<WORKSPACE>` 与 `<HOME>`。
> 规则、未改动的量、以及原始形态见 [`../SCRUB-NOTICE.md`](../SCRUB-NOTICE.md)。
