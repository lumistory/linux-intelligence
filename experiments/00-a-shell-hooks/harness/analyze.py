#!/usr/bin/env python3
"""Turn results/tNN.<shell>.events.jsonl into a fact table for REPORT.md.

No interpretation here: one row per (case, shell) with the numbers and strings
that were actually recorded, plus a verdict computed from them by rule.
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RES = os.path.join(ROOT, "results")
SHELLS = ["bash", "zsh", "fish"]
CASES = ["t%02d" % i for i in range(1, 14)]


def load(case, shell):
    path = os.path.join(RES, "%s.%s.events.jsonl" % (case, shell))
    evs = []
    try:
        with open(path) as f:
            for ln in f:
                ln = ln.strip()
                if not ln:
                    continue
                try:
                    evs.append(json.loads(ln))
                except ValueError:
                    evs.append({"_bad": ln})
    except OSError:
        return None
    return evs


def fin(evs):
    return [e for e in evs if e.get("type") == "CommandFinished"]


def is_load(ev):
    c = (ev.get("payload") or {}).get("command") or ""
    return ("adapters/" in c) or c.startswith(".") or c.startswith("source ")


def rows(case, shell):
    evs = load(case, shell)
    if evs is None:
        return {"case": case, "shell": shell, "status": "NOT TESTED",
                "note": "no events file"}
    f = fin(evs)
    user = [e for e in f if not is_load(e)]
    return {"case": case, "shell": shell, "n_events": len(f),
            "n_user": len(user),
            "cmds": [(e["payload"].get("command"),
                      e["payload"].get("exit_status")) for e in user],
            "evs": evs}


def main():
    out = {}
    for case in CASES:
        for shell in SHELLS:
            r = rows(case, shell)
            r.pop("evs", None)
            out["%s.%s" % (case, shell)] = r
    dst = os.path.join(RES, "analysis.json")
    with open(dst, "w") as f:
        json.dump(out, f, indent=1, sort_keys=True)
    # compact console table
    for case in CASES:
        line = [case.rjust(4)]
        for shell in SHELLS:
            r = out["%s.%s" % (case, shell)]
            line.append("%s:%s/%s" % (shell[0], r.get("n_user", "-"),
                                      r.get("n_events", "-")))
        print("  ".join(line))
    print("wrote", dst)
    return 0


if __name__ == "__main__":
    sys.exit(main())
