#!/usr/bin/env python3
"""Cost of the adapter's own hook body, excluding the pty round trip.

The prompt-to-prompt figures in results/latency/ include the terminal's own
paint time and the harness's read latency, which dominate. To answer "how much
does the hook itself add", time the hook body from *inside* the shell using
EPOCHREALTIME around the adapter's PROMPT_COMMAND function.

Result per shell is the median of the per-command microseconds.
"""
import json
import os
import select
import signal
import statistics
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RES = os.path.join(ROOT, "results")
ADAPT = os.path.join(ROOT, "adapters")
OUTFILE = "/tmp/li-phase0-a-hookcost.txt"
ZSH = "<WORKSPACE>/experiments/toolchain/root/usr/bin/zsh"

INSTALL = {
    "bash": ". %s/bash.sh" % ADAPT,
    "zsh": "source %s/zsh.zsh" % ADAPT,
    "fish": "source %s/fish.fish" % ADAPT,
}


def wrap_code(shell):
    """Re-wrap the adapter's finish hook so it times itself.

    The adapter prepends its own function to PROMPT_COMMAND, so the timer has to
    be spliced around that function rather than replacing PROMPT_COMMAND -- a
    plain overwrite simply disables the adapter and measures nothing.
    """
    o = OUTFILE
    if shell == "bash":
        return ("PROMPT_COMMAND='__a=$EPOCHREALTIME; _li_finish; "
                "__b=$EPOCHREALTIME; "
                "__d=$(( (${__b/./}-${__a/./})/1000 )); "
                "printf \"%s\\n\" \"$__d\" >> " + o + "'")
    if shell == "zsh":
        return ("_li_now(){ local i=${EPOCHREALTIME%%.*} f=${EPOCHREALTIME#*.}; "
                "print -rn -- $(( i*1000000 + ${f[1,6]}*1 )) }; "
                "precmd(){ local a=$(_li_now); _li_precmd; local b=$(_li_now); "
                "print $(( b-a )) >> " + o + " }")
    return ("function _li_timed --on-event fish_postexec; "
            "set -l a (date +%s%N); _li_postexec $argv; "
            "set -l b (date +%s%N); "
            "echo (math \"($b - $a) / 1000\") >> " + o + "; end")


def run(shell, n=40):
    import pty
    ev_r = ev_w = -1
    ev_r, ev_w = os.pipe()          # live sink: the realistic case
    pid, fd = pty.fork()
    if pid == 0:
        try:
            signal.signal(signal.SIGPIPE, signal.SIG_DFL)
            if ev_r == 3:
                m = os.dup(ev_r)
                os.close(ev_r)
                ev_r = m
            os.dup2(ev_w, 3)
            os.set_inheritable(3, True)
            if ev_w != 3:
                os.close(ev_w)
            if ev_r != 3:
                os.close(ev_r)
            env = dict(os.environ)
            env.update({"TERM": "dumb", "HISTFILE": os.devnull,
                        "LC_ALL": "C.UTF-8"})
            if shell == "bash":
                a = ["/usr/bin/bash", "--norc", "-i"]
            elif shell == "zsh":
                a = [ZSH, "-f", "-i"]
            else:
                a = ["/usr/bin/fish", "--no-config", "-i"]
            os.execvpe(a[0], a, env)
        except BaseException:
            os._exit(111)
    os.close(ev_w)
    os.set_blocking(fd, False)
    os.set_blocking(ev_r, False)
    buf = bytearray()

    def rd(t):
        end = time.monotonic() + t
        while time.monotonic() < end:
            r, _, _ = select.select([fd, ev_r], [], [], 0.02)
            for f in r:
                try:
                    d = os.read(f, 65536)
                except OSError:
                    continue
                if f == fd:
                    buf.extend(d)

    try:
        rd(1.4)
        os.write(fd, (INSTALL[shell] + "\r").encode())
        rd(1.0)
        if shell == "fish":
            # the wrapper listens on the same event; drop the adapter's handler
            os.write(fd, b"functions -e _li_postexec\r")
        wrap = wrap_code(shell)
        os.write(fd, (wrap + "\r").encode())
        rd(1.2)
        open(OUTFILE, "w").close()
        for i in range(n):
            os.write(fd, ("true %d\r" % i).encode())
            rd(0.07)
        rd(0.5)
    finally:
        try:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
        except OSError:
            pass
        for f in (fd, ev_r):
            try:
                os.close(f)
            except OSError:
                pass
    vals = []
    try:
        for line in open(OUTFILE):
            line = line.strip()
            if line.isdigit():
                vals.append(int(line))
            elif line.lstrip("-").isdigit():
                vals.append(abs(int(line)))
    except OSError:
        pass
    return vals


def main():
    res = {}
    for shell in (sys.argv[1:] or ["bash", "zsh", "fish"]):
        vals = run(shell)
        if vals:
            r = {"n": len(vals), "median_us": statistics.median(vals),
                 "min_us": min(vals), "max_us": max(vals),
                 "p99_us": sorted(vals)[int(0.99 * (len(vals) - 1))]}
            res[shell] = r
            print("%-5s hook body cost median=%s us  min=%s max=%s n=%d"
                  % (shell, r["median_us"], r["min_us"], r["max_us"], r["n"]))
        else:
            res[shell] = {"error": "no samples"}
            print("%-5s NO SAMPLES" % shell)
    path = os.path.join(RES, "latency", "hookcost.json")
    with open(path, "w") as f:
        json.dump(res, f, indent=1, sort_keys=True)
    print("wrote", path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
