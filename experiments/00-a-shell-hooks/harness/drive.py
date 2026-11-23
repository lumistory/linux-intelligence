#!/usr/bin/env python3
"""pty driver for experiment 00-a.

Why a pty: prompt hooks (PS0 / PROMPT_COMMAND / preexec / fish_preexec) only
exist when the shell is interactive *and* owns a terminal. A pipe would disable
half of what we are measuring.

Two output channels are kept strictly separate:
  * terminal bytes      -> results/<case>.term.log        (noise, debug)
  * hook JSON on FD 3   -> results/<case>.events.jsonl    (the measurement)

Step language (one op per line, '#' comments):
  RUN  <text>    type text + CR, then wait until the pty is quiet
  SEND <text>    type text + CR, do not wait (PS2 continuations, sleep 30)
  CTRLC          write 0x03 to the master
  KILL           SIGKILL the shell process
  WAIT <secs>    sleep
  MARK <text>    annotate the event stream ({'_mark': ...} line)
"""
import argparse
import json
import os
import pty
import select
import signal
import sys
import time

PROMPT = "\u27e6LI-P\u27e7"
PROMPT2 = "\u27e6LI-P2\u27e7"
DEFAULT_ZSH = os.path.expanduser(
    "~/linux-intelligence/experiments/toolchain/root/usr/bin/zsh")


def shell_argv(shell):
    if shell == "bash":
        return ["/usr/bin/bash", "--norc", "--noprofile", "-i"]
    if shell == "zsh":
        return [os.environ.get("LI_ZSH", DEFAULT_ZSH), "-f", "-i"]
    if shell == "fish":
        # fish ignores $PS1 (prompt lives in fish_prompt) and, under a real
        # TERM, blocks on DA1/OSC11 capability queries. Both solved this way.
        return ["/usr/bin/fish", "--no-config", "-i", "-C",
                "function fish_prompt; printf '%s ' " + PROMPT + "; end"]
    raise SystemExit("unknown shell %r" % shell)


class Driver:
    def __init__(self, shell, prefix):
        self.shell = shell
        self.prefix = prefix
        self.pid = -1
        self.fd = -1
        self.evfd = -1
        self.term = None
        self.events = None
        self.n_events = 0
        self.meta = {}

    def spawn(self):
        argv = shell_argv(self.shell)
        ev_r, ev_w = os.pipe()
        pid, fd = pty.fork()
        if pid == 0:
            try:
                # CPython ignores SIGPIPE (and SIGXFSZ); ignoring is inherited
                # across exec, so without this reset the shell would start with
                # SIGPIPE already ignored and the "dead sink" tests would be
                # measuring the harness instead of the shell.
                signal.signal(signal.SIGPIPE, signal.SIG_DFL)
                signal.signal(signal.SIGXFSZ, signal.SIG_DFL)
                # os.pipe() hands out the two lowest free fds and marks them
                # close-on-exec (PEP 446). Two traps:
                #  a) if the write end already *is* 3, dup2(3,3) is a no-op and
                #     fd 3 keeps CLOEXEC -> dies at exec;
                #  b) the read end may also be 3, so it must be moved aside
                #     before we take fd 3 for the write end.
                tmp_r = ev_r
                if tmp_r == 3:
                    tmp_r = os.dup(3)
                os.dup2(ev_w, 3)
                os.set_inheritable(3, True)
                if ev_w != 3:
                    os.close(ev_w)
                os.close(tmp_r)
                env = dict(os.environ)
                env.update({
                    "TERM": "dumb" if self.shell == "fish" else "xterm-256color",
                    "PS1": PROMPT + " ",
                    "PS2": PROMPT2 + " ",
                    "PROMPT": PROMPT + " ",
                    "LI_SESSION_ID": "li-%s-%d" % (self.shell, os.getpid()),
                    "HISTFILE": os.devnull,
                    "HISTSIZE": "2000",
                    "LC_ALL": "C.UTF-8",
                })
                os.execvpe(argv[0], argv, env)
            except BaseException:
                os._exit(111)
        os.close(ev_w)
        self.pid, self.fd, self.evfd = pid, fd, ev_r
        os.set_blocking(ev_r, False)
        self.term = open(self.prefix + ".term.log", "wb", buffering=0)
        self.events = open(self.prefix + ".events.jsonl", "w")
        self._record_proc()

    def _record_proc(self):
        try:
            with open("/proc/%d/stat" % self.pid) as f:
                st = f.read()
            fields = st[st.rindex(")") + 2:].split()
            self.meta = {"pid": self.pid, "pgrp": int(fields[3]),
                         "sid": int(fields[4])}
        except OSError:
            self.meta = {"pid": self.pid}
        self.events.write(json.dumps({"_shell_proc": self.meta}) + "\n")
        self.events.flush()

    def close(self, hard=False):
        try:
            if not hard:
                os.write(self.fd, b"exit\n")
                self.pump(duration=1.0)
        except OSError:
            pass
        try:
            os.kill(self.pid, signal.SIGKILL)
        except OSError:
            pass
        for _ in range(50):
            try:
                done, _st = os.waitpid(self.pid, os.WNOHANG)
            except ChildProcessError:
                done = self.pid
            if done:
                break
            time.sleep(0.02)
        self.pump(duration=0.25)
        self.events.close()
        self.term.close()
        for fd in (self.fd, self.evfd):
            try:
                os.close(fd)
            except OSError:
                pass

    def pump(self, duration=0.5, quiet=None, hard_timeout=None):
        deadline = time.monotonic() + duration
        if hard_timeout is not None:
            deadline = time.monotonic() + hard_timeout
        last_data = time.monotonic()
        while True:
            remaining = deadline - time.monotonic()
            if quiet is not None and remaining > 0:
                if time.monotonic() - last_data >= quiet:
                    return
                remaining = min(remaining, 0.05)
            if remaining <= 0:
                return
            try:
                r, _, _ = select.select([self.fd, self.evfd], [], [], remaining)
            except InterruptedError:
                continue
            for rfd in r:
                if rfd == self.fd:
                    try:
                        data = os.read(self.fd, 65536)
                    except OSError:
                        return
                    if not data:
                        return
                    self.term.write(data)
                    last_data = time.monotonic()
                else:
                    self._drain_events()
                    last_data = time.monotonic()

    def _drain_events(self):
        while True:
            try:
                chunk = os.read(self.evfd, 262144)
            except (BlockingIOError, InterruptedError):
                return
            except OSError:
                return
            if not chunk:
                return
            for line in chunk.split(b"\n"):
                if not line.strip():
                    continue
                try:
                    obj = json.loads(line)
                except ValueError:
                    obj = {"_unparsable": line.decode("utf-8", "replace")}
                obj["_recv_mono_ns"] = time.monotonic_ns()
                obj["_recv_wall"] = time.strftime("%Y-%m-%dT%H:%M:%SZ",
                                                  time.gmtime())
                self.events.write(json.dumps(obj) + "\n")
                self.n_events += 1
            self.events.flush()

    def send(self, text, cr=True):
        payload = text.encode() + (b"\r" if cr else b"")
        try:
            os.write(self.fd, payload)
        except OSError:
            return False
        return True

    def run(self, text, quiet=0.35, timeout=25.0):
        self.send(text)
        self.pump(duration=timeout, quiet=quiet)

    def replay(self, steps):
        self.run("", quiet=0.6, timeout=3.0)
        for raw in steps:
            line = raw.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            op, _, arg = line.partition(" ")
            op = op.upper()
            if op == "RUN":
                self.run(arg)
            elif op == "SEND":
                self.send(arg)
                self.pump(duration=0.25)
            elif op == "CTRLC":
                self.send("\x03", cr=False)
                self.pump(duration=0.45)
            elif op == "KILL":
                try:
                    os.kill(self.pid, signal.SIGKILL)
                except OSError:
                    pass
                self.pump(duration=0.6)
            elif op == "WAIT":
                self.pump(duration=float(arg or 0.5))
            elif op == "MARK":
                self.events.write(json.dumps({"_mark": arg}) + "\n")
                self.events.flush()
            else:
                raise SystemExit("bad step %r" % line)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shell", required=True, choices=["bash", "zsh", "fish"])
    ap.add_argument("--steps", required=True)
    ap.add_argument("--out", required=True, help="output prefix, no extension")
    ap.add_argument("--no-exit", action="store_true")
    a = ap.parse_args()
    with open(a.steps) as f:
        steps = f.readlines()
    d = Driver(a.shell, a.out)
    d.spawn()
    try:
        d.replay(steps)
    finally:
        d.close(hard=a.no_exit)
    print("shell=%s pid=%s events=%d out=%s"
          % (a.shell, d.pid, d.n_events, a.out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
