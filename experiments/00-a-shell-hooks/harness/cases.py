#!/usr/bin/env python3
"""The 13 mandatory interaction cases, replayed on all three shells.

Each case is a list of (op, arg) steps in the pty driver's step language.
Per-shell syntax differences live in CASES; anything that is not expressible in
a shell is marked SKIP with the reason, and the report must then say NOT TESTED.

Outputs, per case and shell:
  results/tNN.<shell>.steps       the exact steps that were replayed
  results/tNN.<shell>.term.log    raw terminal bytes
  results/tNN.<shell>.events.jsonl raw hook events (nothing filtered)
  results/cases.json              machine-readable summary used by REPORT.md
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RES = os.path.join(ROOT, "results")
ADAPT = os.path.join(ROOT, "adapters")
DRIVE = os.path.join(HERE, "drive.py")

LOAD = {"bash": ". %s/bash.sh" % ADAPT,
        "zsh": "source %s/zsh.zsh" % ADAPT,
        "fish": "source %s/fish.fish" % ADAPT}

# Each entry: case id -> dict(shell -> list of steps) ; "SKIP:<reason>" allowed.
CASES = {
 "t01": ("normal single command"),
 "t02": ("backslash continuation, multi-line command"),
 "t03": ("pipeline"),
 "t04": ("command chain && || ;"),
 "t05": ("builtins: cd / export / echo"),
 "t06": ("shell function"),
 "t07": ("alias"),
 "t08": ("background job &"),
 "t09": ("subshell and command substitution $( )"),
 "t10": ("Ctrl+C interrupts a running command"),
 "t11": ("shell SIGKILLed mid-command -- unclosed event?"),
 "t12": ("large output command"),
 "t13": ("ten sequential echoes: order and loss"),
}

STEPS = {
 "t01": {
  "bash": [("RUN", "echo t01-normal")],
  "zsh":  [("RUN", "echo t01-normal")],
  "fish": [("RUN", "echo t01-normal")],
 },
 "t02": {
  "bash": [("SEND", "echo t02-one \\"), ("SEND", "two"),
           ("RUN", "printf 't02-ml\\n'")],
  "zsh":  [("SEND", "echo t02-one \\"), ("SEND", "two"),
           ("RUN", "printf 't02-ml\\n'")],
  "fish": [("SEND", "echo t02-one \\"), ("SEND", "two"),
           ("RUN", "printf 't02-ml\\n'")],
 },
 "t03": {
  "bash": [("RUN", "printf 'x\\ny\\nz\\n' | wc -l")],
  "zsh":  [("RUN", "printf 'x\\ny\\nz\\n' | wc -l")],
  "fish": [("RUN", "printf 'x\\ny\\nz\\n' | wc -l")],
 },
 "t04": {
  "bash": [("RUN", "true && echo t04-and"),
           ("RUN", "false || echo t04-or"),
           ("RUN", "echo t04-a; echo t04-b"),
           ("RUN", "false")],
  "zsh":  [("RUN", "true && echo t04-and"),
           ("RUN", "false || echo t04-or"),
           ("RUN", "echo t04-a; echo t04-b"),
           ("RUN", "false")],
  "fish": [("RUN", "true; and echo t04-and"),
           ("RUN", "false; or echo t04-or"),
           ("RUN", "echo t04-a; echo t04-b"),
           ("RUN", "false")],
 },
 "t05": {
  "bash": [("RUN", "cd /tmp && echo t05-cd-ok"),
           ("RUN", "export LI_T05=1"),
           ("RUN", "echo t05-export-$LI_T05"),
           ("RUN", "cd /")],
  "zsh":  [("RUN", "cd /tmp && echo t05-cd-ok"),
           ("RUN", "export LI_T05=1"),
           ("RUN", "echo t05-export-$LI_T05"),
           ("RUN", "cd /")],
  "fish": [("RUN", "cd /tmp; and echo t05-cd-ok"),
           ("RUN", "set -gx LI_T05 1"),
           ("RUN", "echo t05-export-$LI_T05"),
           ("RUN", "cd /")],
 },
 "t06": {
  "bash": [("RUN", "t06f() { echo t06-in-function; }"),
           ("RUN", "t06f")],
  "zsh":  [("RUN", "t06f() { echo t06-in-function; }"),
           ("RUN", "t06f")],
  "fish": [("RUN", "function t06f; echo t06-in-function; end"),
           ("RUN", "t06f")],
 },
 "t07": {
  "bash": [("RUN", "alias t06alias='echo t07-alias-body'"),
           ("RUN", "t06alias")],
  "zsh":  [("RUN", "alias t07alias='echo t07-alias-body'"),
           ("RUN", "t07alias")],
  "fish": [("RUN", "alias t07alias='echo t07-alias-body'"),
           ("RUN", "t07alias")],
 },
 "t08": {
  "bash": [("RUN", "sleep 0.3 &"),
           ("RUN", "echo t08-after-bg"),
           ("WAIT", "0.6"),
           ("RUN", "wait")],
  "zsh":  [("RUN", "sleep 0.3 &"),
           ("RUN", "echo t08-after-bg"),
           ("WAIT", "0.6"),
           ("RUN", "wait")],
  "fish": [("RUN", "sleep 0.3 &"),
           ("RUN", "echo t08-after-bg"),
           ("WAIT", "0.6"),
           ("RUN", "wait")],
 },
 "t09": {
  "bash": [("RUN", "( echo t09-subshell )"),
           ("RUN", "echo t09-cmdsub-$(echo substituted)")],
  "zsh":  [("RUN", "( echo t09-subshell )"),
           ("RUN", "echo t09-cmdsub-$(echo substituted)")],
  "fish": [("RUN", "begin; echo t09-subshell; end"),
           ("RUN", "echo t09-cmdsub-(echo substituted)")],
 },
 "t10": {
  "bash": [("RUN", "echo t10-before"), ("SEND", "sleep 30"),
           ("WAIT", "0.5"), ("CTRLC", ""), ("RUN", "echo t10-after")],
  "zsh":  [("RUN", "echo t10-before"), ("SEND", "sleep 30"),
           ("WAIT", "0.5"), ("CTRLC", ""), ("RUN", "echo t10-after")],
  "fish": [("RUN", "echo t10-before"), ("SEND", "sleep 30"),
           ("WAIT", "0.5"), ("CTRLC", ""), ("RUN", "echo t10-after")],
 },
 "t11": {
  "bash": [("RUN", "echo t11-before"), ("SEND", "sleep 30"),
           ("WAIT", "0.5"), ("KILL", "")],
  "zsh":  [("RUN", "echo t11-before"), ("SEND", "sleep 30"),
           ("WAIT", "0.5"), ("KILL", "")],
  "fish": [("RUN", "echo t11-before"), ("SEND", "sleep 30"),
           ("WAIT", "0.5"), ("KILL", "")],
 },
 "t12": {
  "bash": [("RUN", "seq 1 20000 | wc -l"),
           ("RUN", "seq 1 3000 > /dev/null; echo t12-redirect-ok"),
           ("RUN", "echo t12-after-big")],
  "zsh":  [("RUN", "seq 1 20000 | wc -l"),
           ("RUN", "seq 1 3000 > /dev/null; echo t12-redirect-ok"),
           ("RUN", "echo t12-after-big")],
  "fish": [("RUN", "seq 1 20000 | wc -l"),
           ("RUN", "seq 1 3000 > /dev/null; echo t12-redirect-ok"),
           ("RUN", "echo t12-after-big")],
 },
 "t13": {
  "bash": [("RUN", "echo t13-%02d" % i) for i in range(1, 11)],
  "zsh":  [("RUN", "echo t13-%02d" % i) for i in range(1, 11)],
  "fish": [("RUN", "echo t13-%02d" % i) for i in range(1, 11)],
 },
}


def steps_for(case, shell):
    out = [("MARK", "case=%s shell=%s" % (case, shell)),
           ("RUN", LOAD[shell])]
    out.extend(STEPS[case][shell])
    return out


def write_steps(path, steps):
    with open(path, "w") as f:
        for op, arg in steps:
            f.write(("%s %s" % (op, arg)).rstrip() + "\n")


def run(case, shell, timeout=120):
    pref = os.path.join(RES, "%s.%s" % (case, shell))
    steps = steps_for(case, shell)
    write_steps(pref + ".steps", steps)
    args = [sys.executable, DRIVE, "--shell", shell,
            "--steps", pref + ".steps", "--out", pref]
    if case == "t11":
        args.append("--no-exit")
    t0 = os.times()
    try:
        p = subprocess.run(args, capture_output=True, timeout=timeout)
        rc, err = p.returncode, p.stderr.decode()[-400:]
    except subprocess.TimeoutExpired:
        rc, err = -9, "harness timeout"
    return {"case": case, "shell": shell, "rc": rc, "stderr": err,
            "steps": pref + ".steps",
            "term": pref + ".term.log", "events": pref + ".events.jsonl"}


def main():
    os.makedirs(RES, exist_ok=True)
    only_cases = [a for a in sys.argv[1:] if not a.startswith("-")]
    shells = ["bash", "zsh", "fish"]
    results = []
    for case in sorted(CASES):
        if only_cases and case not in only_cases:
            continue
        for shell in shells:
            r = run(case, shell)
            results.append(r)
            ev = 0
            try:
                with open(r["events"]) as f:
                    ev = sum(1 for ln in f if '"payload"' in ln)
            except OSError:
                pass
            print("%s %-5s rc=%-3s events=%-3d %s"
                  % (case, shell, r["rc"], ev, r["stderr"][:60]), flush=True)
    out = os.path.join(RES, "cases.json")
    old = {}
    if os.path.exists(out):
        try:
            old = json.load(open(out))
        except ValueError:
            old = {}
    runs = old.get("runs") or {}
    if not isinstance(runs, dict):
        runs = {}
    for r in results:
        runs["%s.%s" % (r["case"], r["shell"])] = r
    old["runs"] = runs
    with open(out, "w") as f:
        json.dump(old, f, indent=1, sort_keys=True)
    print("wrote", out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
