#!/usr/bin/env python3
"""Prompt-to-prompt latency for experiment 00-a.

Measures wall time from "CR typed" to "next prompt painted on the pty". That
interval contains everything the user waits for: command execution plus all
hook work on the finish side (and start side if the shell has one).

Modes
  empty    hook installed but does nothing                     (baseline)
  devnull  full hook payload written to /dev/null              (sink works)
  dead     full hook payload written to a nonexistent socket   (daemon down)
  live     full hook payload delivered to a real unix socket   (daemon up)

The payload is a realistic ~160-byte JSON event, not an empty string, so the
measurement includes formatting cost.
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
LATDIR = os.path.join(ROOT, "results", "latency")
PROMPT = "\u27e6LI-P\u27e7"
SOCK = "/tmp/li-phase0-a/live.sock"
SQR = chr(39)


def payload(n=160):
    """A realistic, *valid* JSON event of about n bytes."""
    d = {"schema_version": 1, "source": "shell.bash", "session_id": "li-bash-48213",
         "sequence": 7, "ts_wall": "2026-11-23T19:00:00.123Z",
         "type": "CommandFinished",
         "payload": {"command": "ls -la /usr/share/doc | grep -c .",
                     "exit_status": 0, "duration_ms": 12,
                     "cwd": "<WORKSPACE>"},
         "evidence_status": "declared"}
    s = json.dumps(d)
    if len(s) < n:
        d["payload"]["command"] += " /" + "x" * (n - len(s) - 2)
        s = json.dumps(d)
    assert json.loads(s)
    return s


def hook_code(shell, p):
    assert SQR not in p and "`" not in p
    q = SQR
    sink = '{ printf "%s" ' + q + p + q + ' >&3; } 2>/dev/null'
    if shell == "bash":
        return "PROMPT_COMMAND=" + q + sink + "; :" + q
    if shell == "zsh":
        return "precmd() { " + sink + " }; add-zsh-hook precmd precmd"
    return ("function __lat_post --on-event fish_postexec; printf '%s' "
            + q + p + q + " >&3 2>/dev/null; end")


def build(mode, shell):
    if mode == "empty":
        return {"bash": "PROMPT_COMMAND=;:",
                "zsh": "precmd() { :; }",
                "fish": "function __lat_post --on-event fish_postexec; end"}[shell]
    return hook_code(shell, payload())


def argv_for(shell):
    if shell == "bash":
        return ["/usr/bin/bash", "--norc", "--noprofile", "-i"]
    if shell == "zsh":
        return [os.environ.get("LI_ZSH", os.path.expanduser(
            "~/linux-intelligence/experiments/toolchain/root/usr/bin/zsh")),
            "-f", "-i"]
    return ["/usr/bin/fish", "--no-config", "-i", "-C",
            "function fish_prompt; printf '%s ' " + PROMPT + "; end"]


def run_once(shell, code, n_iter):
    import pty
    ev_r, ev_w = os.pipe()
    pid, fd = pty.fork()
    if pid == 0:
        try:
            signal.signal(signal.SIGPIPE, signal.SIG_DFL)   # see drive.py
            signal.signal(signal.SIGXFSZ, signal.SIG_DFL)
            tmp_r = ev_r
            if tmp_r == 3:
                tmp_r = os.dup(3)
            os.dup2(ev_w, 3)
            os.set_inheritable(3, True)
            if ev_w != 3:
                os.close(ev_w)
            os.close(tmp_r)
            env = dict(os.environ)
            # fish 4.9.3 blocks on startup terminal-capability queries
            # (DA1 / OSC11) when TERM is a real terminal type; under TERM=dumb
            # it skips them and renders a plain prompt. Measured, see README.
            env.update({"TERM": "dumb" if shell == "fish" else "xterm-256color",
                        "PS1": PROMPT + " ", "PROMPT": PROMPT + " ",
                        "HISTFILE": os.devnull, "LC_ALL": "C.UTF-8"})
            os.execvpe(argv_for(shell)[0], argv_for(shell), env)
        except BaseException:
            os._exit(111)
    os.close(ev_w)
    os.set_blocking(fd, False)
    buf = bytearray()

    def drain(t):
        end = time.monotonic() + t
        while time.monotonic() < end:
            r, _, _ = select.select([fd, ev_r], [], [], 0.05)
            for f in r:
                try:
                    d = os.read(f, 262144)
                except OSError:
                    d = b""
                if f == fd:
                    buf.extend(d)

    def clear():
        buf.clear()

    def wait_prompt(timeout=6.0):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if PROMPT.encode() in buf:
                return True
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    buf.extend(os.read(fd, 262144))
                except OSError:
                    return False
        return False

    drain(1.2)
    clear()
    os.write(fd, code.encode() + b"\r")
    if not wait_prompt(8.0):
        try:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
        except OSError:
            pass
        os.close(fd)
        os.close(ev_r)
        raise RuntimeError("no prompt after hook load")
    drain(0.4)
    lat = []
    for i in range(n_iter):
        clear()
        t0 = time.monotonic_ns()
        os.write(fd, b"true %d\r" % i)
        ok = wait_prompt(6.0)
        t1 = time.monotonic_ns()
        if not ok:
            break
        lat.append((t1 - t0) // 1000)
    try:
        os.kill(pid, 9)
        os.waitpid(pid, 0)
    except OSError:
        pass
    os.close(fd)
    os.close(ev_r)
    return lat


def pct(vals, p):
    if not vals:
        return None
    k = sorted(vals)
    return k[min(len(k) - 1, int(round((p / 100.0) * (len(k) - 1))))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shells", default="bash,zsh,fish")
    ap.add_argument("--modes", default="empty,devnull,dead,live")
    ap.add_argument("--iters", type=int, default=100)
    ap.add_argument("--out", default=os.path.join(LATDIR, "latency.json"))
    a = ap.parse_args()
    os.makedirs(LATDIR, exist_ok=True)
    os.makedirs("/tmp/li-phase0-a", exist_ok=True)
    if os.path.exists(SOCK):
        os.unlink(SOCK)
    results = {}
    for shell in a.shells.split(","):
        for mode in a.modes.split(","):
            code = build(mode, shell)
            try:
                lat = run_once(shell, code, a.iters)
            except RuntimeError as e:
                results["%s/%s" % (shell, mode)] = {"error": str(e)}
                print("ERROR %s/%s: %s" % (shell, mode, e), file=sys.stderr)
                continue
            r = {"n": len(lat),
                 "median_us": statistics.median(lat) if lat else None,
                 "mean_us": round(statistics.fmean(lat), 1) if lat else None,
                 "p99_us": pct(lat, 99), "min_us": min(lat) if lat else None,
                 "max_us": max(lat) if lat else None, "raw_us": lat}
            results["%s/%s" % (shell, mode)] = r
            print("%-12s n=%-4d median=%8.0f  p99=%8s us"
                  % (shell + "/" + mode, len(lat), r["median_us"] or -1,
                     r["p99_us"]), flush=True)
    with open(a.out, "w") as f:
        json.dump(results, f, indent=1)
    print("wrote", a.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
