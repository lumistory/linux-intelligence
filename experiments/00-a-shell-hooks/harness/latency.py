#!/usr/bin/env python3
"""Prompt-to-prompt latency for experiment 00-a.

Measures wall time from "CR typed" to "next prompt painted on the pty". That
interval contains everything the user waits for: the command itself plus all
hook work on the finish side.

Modes -- each one is a genuinely different sink, not a relabel:
  empty     hook installed but does nothing                  (baseline)
  devnull   payload -> /dev/null, in shell, no fork
  deadsock  payload | li_emit /nonexistent.sock  -> connect fails, one fork
            (this is the real "the daemon is not running" path)
  live      payload | li_emit <live socket>      -> connect + write, one fork
  fd3dead   payload -> FD 3 whose reader has exited, guarded by trap '' PIPE
            (worst case: a sink that died while the shell was running)
"""
import argparse
import json
import os
import select
import signal
import socket
import statistics
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
LATDIR = os.path.join(ROOT, "results", "latency")
EMIT = os.path.join(HERE, "li_emit")
PROMPT = "\u27e6LI-P\u27e7"
SOCK = "/tmp/li-phase0-a/live.sock"
DEAD = "/tmp/li-phase0-a/definitely-not-here.sock"
SQR = chr(39)
ZSH = "<WORKSPACE>/experiments/toolchain/root/usr/bin/zsh"


def payload(n=200):
    d = {"schema_version": 1, "source": "shell.bash", "session_id": "li-25871",
         "sequence": 7, "ts_wall": "2026-11-23T19:00:00.123456Z",
         "type": "CommandFinished",
         "payload": {"command": "ls -la /usr/share/doc | grep -c .",
                     "exit_status": 0, "duration_ms": 12,
                     "cwd": "<WORKSPACE>"},
         "evidence_status": "declared"}
    s = json.dumps(d, separators=(",", ":"))
    if len(s) < n:
        d["payload"]["command"] += " " + "x" * (n - len(s))
        s = json.dumps(d, separators=(",", ":"))
    assert json.loads(s)
    return s


def shq(text):
    """Quote a string for embedding in a single-quoted shell word.

    Plain '...'' wrapping is NOT enough: the payload contains `|`, and if any
    quote is lost the shell silently turns the payload into a pipeline. This bit
    me during development -- the "devnull" mode measured 1.5 ms because
    PROMPT_COMMAND had become `{ printf %s {schema...| grep -c .} ... }`, i.e.
    the JSON was executed as code. The payload is always passed as a shell
    *variable* now, never spliced into the command text.
    """
    return SQR + text.replace(SQR, SQR + '"' + SQR + '"' + SQR) + SQR


def finish_code(shell, mode):
    """One shell line installing the finish-side hook for `mode`.

    The payload is stored in $__li_p first, then the hook only ever references
    that variable, so no payload byte can change the command's structure.
    """
    q = SQR
    if mode == "empty":
        return {"bash": "PROMPT_COMMAND=:", "zsh": "precmd(){ : }",
                "fish": "function __lat_post --on-event fish_postexec; end"}[shell]
    assign = "__li_p=" + shq(payload()) + "; "
    if mode == "devnull":
        body = 'printf "%s" "$__li_p" >/dev/null'
    elif mode == "deadsock":
        body = 'printf "%s" "$__li_p" | ' + shq(EMIT) + " " + shq(DEAD)
    elif mode == "live":
        body = 'printf "%s" "$__li_p" | ' + shq(EMIT) + " " + shq(SOCK)
    elif mode == "fd3dead":
        body = ('trap "" PIPE; printf "%s" "$__li_p" >&3 2>/dev/null; '
                "trap - PIPE")
    else:
        raise SystemExit("bad mode " + mode)
    if shell == "bash":
        return assign + "PROMPT_COMMAND=" + q + "{ " + body + "; } 2>/dev/null; :" + q
    if shell == "zsh":
        return assign + "precmd() { { " + body + " } 2>/dev/null }"
    return (assign + "function __lat_post --on-event fish_postexec; "
            + body + " 2>/dev/null; end")


def install_code(shell, mode):
    """Wrap the finish body in the shell's own hook mechanism."""
    if mode == "empty":
        return finish_code(shell, mode)
    body = finish_code(shell, mode)
    if shell == "zsh":
        return "precmd() { " + body + " }; add-zsh-hook precmd precmd"
    return body


def argv_for(shell):
    if shell == "bash":
        return ["/usr/bin/bash", "--noprofile", "--rcfile", "/tmp/li-phase0-a/rc/bashrc", "-i"]
    if shell == "zsh":
        return [os.environ.get("LI_ZSH", ZSH), "-f", "-i"]
    return ["/usr/bin/fish", "--no-config", "-i", "-C",
            "function fish_prompt; printf '%s ' " + PROMPT + "; end"]


class Session:
    def __init__(self, shell, mode, iters, install=None, fd3=None):
        self.shell, self.mode, self.iters = shell, mode, iters
        self.install_override = install
        # fd3: None = no FD 3, "dead" = pipe whose reader is gone,
        #      "live" = pipe drained by the parent (a stand-in for the daemon)
        self.fd3 = fd3
        self.lat = []
        self.err = None

    def run(self):
        import pty
        ev_r = ev_w = -1
        if self.fd3:
            ev_r, ev_w = os.pipe()
            if self.fd3 == "dead":
                # reader gone: every write into FD 3 hits a closed pipe, which
                # is exactly what happens when the daemon dies mid-session.
                os.close(ev_r)
                ev_r = -1
        parent_ev_r = ev_r
        pid, fd = pty.fork()
        if pid == 0:
            try:
                signal.signal(signal.SIGPIPE, signal.SIG_DFL)
                signal.signal(signal.SIGXFSZ, signal.SIG_DFL)
                if ev_w >= 0:
                    # os.pipe() returns the two lowest free fds, so ev_r is
                    # often already 3. dup2(ev_w, 3) would then clobber the read
                    # end and the close(tmp_r) below would fail with EBADF --
                    # which aborted the child with exit 111 and made every
                    # adapter measurement look like "no prompt". Move the read
                    # end out of the way first, then claim fd 3.
                    if ev_r == 3:
                        moved = os.dup(ev_r)
                        os.close(ev_r)
                        ev_r = moved
                    os.dup2(ev_w, 3)
                    os.set_inheritable(3, True)
                    if ev_w != 3:
                        os.close(ev_w)
                env = dict(os.environ)
                env.update({"TERM": "dumb" if self.shell == "fish"
                            else "xterm-256color", "HISTFILE": os.devnull,
                            "LC_ALL": "C.UTF-8"})
                # bash ignores an inherited PS1 (it overwrites it with its
                # default), so the prompt is forced per shell below.
                if self.shell != "bash":
                    env["PS1"] = PROMPT + " "
                env["PROMPT"] = PROMPT + " "
                a = argv_for(self.shell)
                os.execvpe(a[0], a, env)
            except BaseException:
                os._exit(111)
        ev_r = parent_ev_r
        if ev_w >= 0:
            os.close(ev_w)
        if ev_r >= 0:
            os.set_blocking(ev_r, False)
        os.set_blocking(fd, False)
        buf = bytearray()

        def drain(t):
            """Read until t; keeps the event pipe drained so the shell never
            blocks on a full pipe buffer (which would look like hook latency)."""
            fds = [fd] + ([ev_r] if ev_r >= 0 else [])
            end = time.monotonic() + t
            while time.monotonic() < end:
                try:
                    r, _, _ = select.select(fds, [], [], 0.05)
                except OSError:
                    return
                for f in r:
                    try:
                        data = os.read(f, 262144)
                    except OSError:
                        continue
                    if f == fd:
                        buf.extend(data)

        M = PROMPT.encode()

        # Per-shell "the prompt has really been repainted" signature, sampled
        # from the actual pty output of each shell (results/latency/
        # prompt-signatures.log). bash brackets every prompt with the
        # bracketed-paste escape; zsh pads and terminates the previous line with
        # " \r"; fish terminates with "\r\r".
        # bash: never repaints a prompt before running a command, and the
        # settle() before each sample removes any stale prompt, so the bare
        # marker is unambiguous. (Its bracketed-paste escape is NOT reliable:
        # it is absent when the shell is started with a minimal rcfile.)
        # zsh: pads the line and ends it with "\r" before the prompt.
        # fish: ends the previous line with "\r\r" before repainting.
        SIG = {
            "bash": (M,),
            "zsh":  (b"\r" + M, b"\r\n" + M),
            "fish": (b"\r\r" + M,),
        }[self.shell]

        def wait_prompt(timeout=8.0):
            """Block until the prompt that FOLLOWS the command is painted.

            The echo of the typed text also contains the marker for zsh/fish
            (their prompt is redrawn around it), so the test is a per-shell
            signature, not "marker present". Verified with `sleep 0.5`, which
            must be detected no earlier than 0.5 s.
            """
            end = time.monotonic() + timeout
            while time.monotonic() < end:
                if any(sig in buf for sig in SIG):
                    return True
                fds = [fd] + ([ev_r] if ev_r >= 0 else [])
                try:
                    r, _, _ = select.select(fds, [], [], 0.02)
                except OSError:
                    return False
                for f in r:
                    try:
                        data = os.read(f, 262144)
                    except OSError:
                        continue
                    if f == fd:
                        buf.extend(data)
            return False

        drain(1.4)
        buf.clear()
        code = self.install_override if self.install_override is not None \
            else install_code(self.shell, self.mode)
        os.write(fd, code.encode() + b"\r")
        if not wait_prompt(10.0):
            self.err = "no prompt after hook install"
            self._kill(pid, fd, ev_r)
            return self.lat
        drain(0.4)
        buf.clear()
        def settle(quiet=0.03, limit=1.0):
            """Read until the pty has been silent, so the next sample starts clean.

            Without this the detector can match a prompt that was already in
            flight when buf was cleared, which reads as a ~20 us "command" --
            a pty round trip cannot be that fast.
            """
            end = time.monotonic() + limit
            while time.monotonic() < end:
                fds = [fd] + ([ev_r] if ev_r >= 0 else [])
                try:
                    r, _, _ = select.select(fds, [], [], quiet)
                except OSError:
                    return
                if not r:
                    return
                for f in r:
                    try:
                        os.read(f, 262144)
                    except OSError:
                        continue

        probe = getattr(self, "probe", None)
        for i in range(self.iters):
            settle()
            buf.clear()
            cmd = (probe + "\r").encode() if probe else b"true %d\r" % i
            t0 = time.monotonic_ns()
            os.write(fd, cmd)
            if not wait_prompt(6.0):
                break
            self.lat.append((time.monotonic_ns() - t0) // 1000)
        self._kill(pid, fd, ev_r)
        return self.lat

    @staticmethod
    def _kill(pid, fd, ev_r=-1):
        try:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
        except OSError:
            pass
        for f in (fd, ev_r):
            if f >= 0:
                try:
                    os.close(f)
                except OSError:
                    pass


def selftest(shells):
    """Prove the prompt detector waits for the command, not for its echo.

    `sleep 0.5` must be reported at >= 500 ms and `true` at ~0 ms. If the
    detector fired on the echo, both would be ~0.
    """
    lines = ["# detection self-test (generated by harness/latency.py --selftest)",
             "# A detector that fires on the echo of the typed text would report",
             "# `sleep 0.5` as ~0 ms. It must report >= 500 ms.", ""]
    ok_all = True
    for shell in shells:
        for cmd, minimum in (("sleep 0.5", 480), ("true", 0), ("sleep 1.2", 1150)):
            lat = run_single(shell, cmd)
            verdict = "PASS" if lat is not None and lat >= minimum else "FAIL"
            if verdict == "FAIL":
                ok_all = False
            lines.append("%-5s %-11s detected=%6s us  expected>=%5d  %s"
                         % (shell, cmd,
                            "None" if lat is None else "%.0f" % lat,
                            minimum, verdict))
    lines.append("")
    lines.append("OVERALL: %s" % ("PASS" if ok_all else "FAIL"))
    path = os.path.join(LATDIR, "detection-selftest.log")
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")
    print("\n".join(lines))
    print("wrote", path)
    return 0 if ok_all else 1


def run_single(shell, cmd):
    """Time one command with the standard detector; return us or None."""
    s = Session(shell, "empty", 1)
    s.probe = cmd          # consumed by Session.run via self.probe
    lat = s.run()
    return lat[0] if lat else None


def pct(v, p):
    if not v:
        return None
    k = sorted(v)
    return k[min(len(k) - 1, int(round((p / 100.0) * (len(k) - 1))))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shells", default="bash,zsh,fish")
    ap.add_argument("--modes", default="empty,devnull,deadsock,live")
    ap.add_argument("--iters", type=int, default=100)
    ap.add_argument("--repeats", type=int, default=3,
                    help="independent sessions per (shell,mode); report the "
                         "median of the per-session medians")
    ap.add_argument("--selftest", action="store_true",
                    help="verify the prompt detector against `sleep 0.5`")
    ap.add_argument("--out", default=os.path.join(LATDIR, "latency.json"))
    a = ap.parse_args()
    os.makedirs(LATDIR, exist_ok=True)
    os.makedirs("/tmp/li-phase0-a", exist_ok=True)
    if a.selftest:
        return selftest(a.shells.split(","))
    if not os.access(EMIT, os.X_OK):
        raise SystemExit("build harness/li_emit first (see run.sh)")
    res = {}
    for shell in a.shells.split(","):
        for mode in a.modes.split(","):
            # A single session is not enough: a pty measurement is exposed to
            # scheduling noise, and one bad run shifted the median by an order
            # of magnitude during development. Each repeat is an independent
            # shell process; the reported figure is the median of the repeats.
            meds, allraw, err = [], [], None
            for _ in range(a.repeats):
                sess = Session(shell, mode, a.iters)
                lat = sess.run()
                if sess.err:
                    err = sess.err
                    break
                if lat:
                    meds.append(statistics.median(lat))
                    allraw.extend(lat)
            if err or not meds:
                res["%s/%s" % (shell, mode)] = {"error": err or "no samples"}
                print("ERROR %s/%s %s" % (shell, mode, err or "no samples"),
                      flush=True)
                continue
            r = {"n": len(allraw), "repeats": len(meds),
                 "median_us": statistics.median(meds),
                 "median_per_session_us": [round(x) for x in meds],
                 "mean_us": round(statistics.fmean(allraw), 1),
                 "p99_us": pct(allraw, 99), "p95_us": pct(allraw, 95),
                 "min_us": min(allraw), "max_us": max(allraw),
                 "raw_us": allraw}
            res["%s/%s" % (shell, mode)] = r
            print("%-6s %-9s n=%-4d median=%7.0f p99=%7s us  sessions=%s"
                  % (shell, mode, len(allraw), r["median_us"], r["p99_us"],
                     r["median_per_session_us"]), flush=True)
    with open(a.out, "w") as f:
        json.dump(res, f, indent=1, sort_keys=True)
    print("wrote", a.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
