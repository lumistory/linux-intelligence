# Superseded draft runs

These are raw outputs from earlier iterations of `run.sh` and the prototype,
kept for provenance only. **Nothing in `REPORT.md` cites this directory.**

They are here because several of them are the *evidence for the defects
recorded in REPORT.md §6.1 and §6.3*: the wrong `PR_GET_NO_NEW_PRIVS` reading,
the unaudited oversize frame, the allowlist test that actually measured the
filesystem gate, and the `setpriv --clear-groups` false reading. Deleting them
would erase the record of how each error was found.

Do not read numbers from this directory as current results.
