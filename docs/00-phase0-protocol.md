# Phase 0 Experimental Protocol

All Phase 0 experiments MUST follow this document. Deviations must be recorded
in the experiment report under "Deviations".

## 0. Ground rules

1. No result may be written unless the command that produced it was actually run.
2. Every experiment directory contains:
   - `README.md` — goal, hypothesis, method (written *before* running).
   - `run.sh` — reproducible driver, exits non-zero on failure.
   - `results/` — raw captured output, timestamped, never edited by hand.
   - `REPORT.md` — the 8-section report below.
3. A hypothesis that fails is a **valid result**. Record it verbatim.
4. Anything not measured is marked `NOT MEASURED`, never estimated.
5. Experiments never touch files outside
   `$HOME/linux-intelligence/experiments/<exp>/` and `/tmp/li-phase0-*`.

## 1. Report format (REPORT.md, mandatory sections)

```
## 1. Objective
## 2. Hypothesis
## 3. Method
## 4. Raw results        (paste or reference results/*.log paths)
## 5. Analysis           (what the results actually mean)
## 6. Known limitations  (explicit, includes what was NOT tested)
## 7. Conclusion         (PASS / PASS-WITH-LIMITS / FAIL)
## 8. Gate               (may Phase 1 implement this? yes/no + conditions)
```

## 2. Shared event envelope (draft, all experiments emit this)

```json
{
  "schema_version": 1,
  "event_id": "uuid-v4-or-uuid-v7",
  "source": "shell.bash | shell.zsh | shell.fish | script | pty | proc | fs | recovery | policy",
  "session_id": "opaque string, stable for one shell/pty/script lifetime",
  "sequence": 0,
  "ts_mono_ns": 0,
  "ts_wall": "2026-11-23T18:00:00.000Z",
  "type": "CommandStarted",
  "payload": {},
  "evidence_status": "observed | declared | inferred"
}
```

- `evidence_status = observed` — read from the kernel (procfs, inotify, SO_PEERCRED)
  or is a first-hand fact of the emitting process.
- `evidence_status = declared` — self-reported by a shell hook or a user-supplied
  string; may simply be wrong (user typed a lie, hook fired twice).
- `evidence_status = inferred` — produced by correlation, never a raw event.

## 3. Capability declaration

Every adapter/provider must expose a machine-readable capability record:

```json
{"adapter":"bash","version":"5.3","capabilities":{
  "command_text":true,"start_time":true,"end_time":true,"exit_status":true,
  "cwd":true,"multi_line":true,"pipelines":"single-event","builtins":true,
  "functions":true,"aliases":true,"background":false,"subshell":"unknown",
  "ctrl_c_reported":true,"internal_script_commands":false}}
```

Values are `true | false | "unknown" | "partial:<note>" | "single-event" | "per-command"`.
A capability that was not experimentally verified must be `"unknown"`.

## 4. Directory ownership

Each experiment owns exactly one `experiments/<id>/` directory. No experiment
modifies another experiment's files.

## 5. Test environment

- Host: Arch Linux, kernel 7.2.8-zen, btrfs `/`, systemd 262, x86_64.
- Local zsh (not installed system-wide) at
  `experiments/toolchain/root/usr/bin/zsh` (zsh 5.9.2, extracted from the Arch package).
- Cross-distro work uses rootless podman images, never the host.
