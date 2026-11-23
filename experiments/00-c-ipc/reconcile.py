#!/usr/bin/env python3
"""Reconcile a sender's own accounting against the daemon's journal.

Reads the run log given as argv[1], finds each `sender` and `audit` JSON line,
and cross-checks: events the client dropped on purpose vs sequences missing
from the journal. Anything that does not add up is printed as MISMATCH.
"""
import json
import sys


def load(path):
    out = []
    for line in open(path, encoding="utf-8", errors="replace"):
        line = line.strip()
        if line.startswith("{") and line.endswith("}"):
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return out


def main():
    recs = load(sys.argv[1])
    senders = [r for r in recs if r.get("mode") == "sender"]
    audits = {r.get("session"): r for r in recs if r.get("mode") == "audit"}
    if not senders:
        print("no sender records found")
        return 1
    bad = 0
    for s in senders:
        sess = s.get("session")
        a = audits.get(sess)
        if not a:
            print(f"{sess}: MISMATCH - no audit record")
            bad += 1
            continue
        produced = s["produced"]
        dropped = s["dropped_client_queue_full"]
        written = s["written_to_socket"]
        accepted = s.get("server_accepted_count")
        present = a["present"]
        lost = a["lost"]
        print(
            f"{sess}: produced={produced} client_dropped={dropped} "
            f"written={written} server_accepted={accepted} "
            f"journal_present={present} journal_lost={lost}"
        )
        # Truth 1: whatever the daemon journalled is exactly what it accepted.
        # Only valid when the daemon did not restart: its per-session counters
        # live in memory and a SIGKILL takes them with it, so after a restart
        # `server_accepted` covers only the post-restart window and is a lower
        # bound. The journal, not the counter, is the durable record.
        restarted = s.get("reconnect_succeeded", 0) > 0
        if restarted and accepted is not None:
            print(
                f"{sess}: note server_accepted={accepted} is post-restart only "
                f"(daemon was restarted {s.get('reconnect_succeeded')}x); "
                "journal is authoritative"
            )
        elif accepted is not None and accepted != present:
            print(
                f"{sess}: MISMATCH server_accepted({accepted}) != journal_present({present})"
            )
            bad += 1
        # Truth 2: no event is lost inside the transport: everything written
        # either arrived or is still unacked because the connection died.
        if accepted is not None and written < accepted:
            print(f"{sess}: IMPOSSIBLE written({written}) < accepted({accepted})")
            bad += 1
        # Truth 3: the client's deliberate drops must be a subset of the
        # journal's gaps - losses can never be fewer than what the client threw
        # away itself.
        if dropped > lost:
            print(
                f"{sess}: MISMATCH client_dropped({dropped}) > journal_lost({lost}) - "
                "the journal contains events the client says it never sent"
            )
            bad += 1
        # An event is "unexplained" only if the journal is missing something the
        # client neither dropped on purpose nor counts as unwritten.
        unexplained = max(0, lost - dropped)
        if unexplained:
            print(f"{sess}: MISMATCH unexplained_loss={unexplained} - journal gaps "
                  "that the client does not account for")
            bad += 1
        print(
            f"{sess}: lost_explained_by_client={dropped} unexplained={unexplained} "
            f"written_minus_journaled={written - present}"
        )
    print("RECONCILE", "OK" if bad == 0 else f"{bad} MISMATCH(ES)")
    return 0 if bad == 0 else 2


if __name__ == "__main__":
    sys.exit(main())
