# ADR 0004 — Shell adapter boundary

- Status: proposed
- Date: 2026-11-23

## Context

Bash, zsh and fish expose completely different hook mechanisms. The core must
not learn shell quirks.

## Decision (proposed)

Each shell gets a self-contained adapter that installs hooks, emits the shared
event envelope, and declares a machine-readable capability record. The core
treats every shell identically and treats any capability marked `unknown` as
unavailable, never as "probably works".

Baseline direction from 0-A:

- bash: `PROMPT_COMMAND` is the primary source (real exit status, `history 1`
  yields the full command line including backslash-continued lines). `PS0`
  expands before the command runs and therefore reports the previous `$?` and
  the previous `$BASH_COMMAND`; at best it marks "about to execute".
- zsh: `preexec` / `precmd` via `add-zsh-hook`.
- fish: `fish_preexec` / `fish_postexec`.

## Alternatives considered

- `DEBUG` trap for bash: fires per simple command, inside functions and
  subshells, interferes with xtrace and can change program behaviour. Rejected.
- A generic shell wrapper forcing every shell into a managed PTY: violates the
  requirement not to change the user's terminal.

## Consequences

- "Adapter declares a capability" is not "capability verified". Only Phase 0
  results promote a capability from `unknown`.
- Non-interactive shells and scripts are out of scope for this adapter family.
