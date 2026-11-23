#!/usr/bin/env python3
"""Latency of the REAL adapters (not the synthetic probe snippets).

Question: what does the shipped adapter cost per command, and what does it cost
when the daemon it talks to is not there? Two sinks are compared, because they
are the two ways an adapter can reach a daemon at all (results/probe-unix-socket-open.log):

  fd3-dead   FD 3 is a pipe whose reader exited; the adapter's guarded write
             fails immediately. Nothing is forked.  <-- the recommended design
  helper     FD 3 is absent, the adapter pipes its JSON to `harness/li_emit`
             pointed at a nonexistent socket (connect() fails, no dial).

Both must stay in the same order of magnitude as an empty hook for the
"daemon down must not slow the shell" claim to hold.
"""
import argparse
import json
import os
import select
import signal
import statistics
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RES = os.path.join(ROOT, "results", "latency")
PROMPT = "\u27e6LI-P\u27e7"
ADAPT = os.path.join(ROOT, "adapters")
EMIT = os.path.join(HERE, "li_emit")
ZSH = "<WORKSPACE>/experiments/toolchain/root/usr/bin/zsh"


def argv_for(shell):
    if shell == "bash":
        return ["/usr/bin/bash", "--noprofile", "--rcfile", "/tmp/li-phase0-a/rc/bashrc", "-i"]
    if shell == "zsh":
        return [ZSH, "-f", "-i"]
    return ["/usr/bin/fish", "--no-config", "-i", "-C",
            "function fish_prompt; printf '%s ' " + PROMPT + "; end"]


def mode_setup(shell, mode):
    """Return (child_fd3_kind, install_line).

    baseline : hooks removed/disabled      -> the shell's own prompt cost
    fd3dead  : real adapter, FD 3 is a pipe whose reader exited; the adapter's
               guarded write fails at once and no process is forked
    fd3ok    : real adapter, FD 3 is a live pipe drained by the harness
               (stand-in for "the daemon is up and reading")
    """
    if mode == "baseline":
        return "none", {"bash": "PROMPT_COMMAND=:", "zsh": "precmd(){ : }",
                        "fish": "function __x --on-event fish_postexec; end"}[shell]
    if mode == "fd3dead":
        return "dead", LOAD[shell]
    if mode == "fd3ok":
        return "ok", LOAD[shell]
    raise SystemExit("bad mode")


LOAD = {"bash": ". %s/bash.sh" % ADAPT, "zsh": "source %s/zsh.zsh" % ADAPT,
        "fish": "source %s/fish.fish" % ADAPT}


def measure(shell, mode, iters):
    """Measure with the shared Session (same detector, same settle logic).

    Reimplementing the pty loop here is what made the first attempt wrong: it
    lacked the per-shell prompt signature and the pre-sample settle, so it timed
    the echo instead of the command.
    """
    import importlib.util
    spec = importlib.util.spec_from_file_location(
        "_lat", os.path.join(HERE, "latency.py"))
    lat_mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(lat_mod)
    sink, install = mode_setup(shell, mode)
    sess = lat_mod.Session(shell, "empty", iters, install=install,
                           fd3=None if sink == "none" else sink)
    lat = sess.run()
    return lat, sess.err is None


def pct(v, p):
    if not v:
        return None
    k = sorted(v)
    return k[min(len(k) - 1, int(round((p / 100.0) * (len(k) - 1))))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shells", default="bash,zsh,fish")
    ap.add_argument("--modes", default="baseline,fd3dead")
    ap.add_argument("--iters", type=int, default=100)
    ap.add_argument("--out", default=os.path.join(RES, "adapters.json"))
    a = ap.parse_args()
    os.makedirs(RES, exist_ok=True)
    out = {}
    for shell in a.shells.split(","):
        for mode in a.modes.split(","):
            lat, ok = measure(shell, mode, a.iters)
            key = "%s/%s" % (shell, mode)
            if not lat:
                out[key] = {"error": "no samples (prompt_ok=%s)" % ok}
                print("ERROR %-14s prompt_ok=%s" % (key, ok), flush=True)
                continue
            r = {"n": len(lat), "median_us": statistics.median(lat),
                 "mean_us": round(statistics.fmean(lat), 1),
                 "p99_us": pct(lat, 99), "p95_us": pct(lat, 95),
                 "min_us": min(lat), "max_us": max(lat), "raw_us": lat}
            out[key] = r
            print("%-14s n=%-4d median=%8.0f p99=%8s us"
                  % (key, len(lat), r["median_us"], r["p99_us"]), flush=True)
    with open(a.out, "w") as f:
        json.dump(out, f, indent=1, sort_keys=True)
    print("wrote", a.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
