# ADR 0006 — Recovery provider abstraction

- Status: proposed
- Date: 2026-11-23

## Context

"Restore" means different things for git worktrees, btrfs subvolumes, LVM
snapshots, snapper snapshots and plain file copies. Pretending they are
interchangeable would let the system promise safety it does not have.

## Decision (proposed)

Every provider implements `capabilities`, `create_checkpoint`, `inspect`,
`restore`, `verify`, plus a mandatory `caveats` list in its capability record:

- file-level backup is not a filesystem snapshot (no atomic point in time, no
  open-file consistency);
- git restores tracked files only, and only as of a commit;
- btrfs/snapper snapshots are cheap and atomic per subvolume but do not imply
  application consistency (databases, VMs);
- LVM snapshots cost space and degrade the origin volume's write performance.

## Consequences

- `restore` requires scope preview, overwrite check, explicit approval,
  post-hoc verification and a recorded failure path. Recovery is itself
  policy-checked and audited.
- Phase 1 implements no destructive restore; experiments run only in a scratch
  subvolume or directory.
