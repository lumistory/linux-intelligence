#!/usr/bin/env python3
"""39-case integration matrix for the product shell hooks and li-hook."""
import argparse
import importlib.util
import json
import os
import pty
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HARNESS = ROOT / "experiments/00-a-shell-hooks/harness"
spec = importlib.util.spec_from_file_location("cases", HARNESS / "cases.py")
cases = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cases)
spec = importlib.util.spec_from_file_location("drive", HARNESS / "drive.py")
drive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drive)
SHELLS = ("bash", "zsh", "fish")
HOOK = None


class ProductDriver(drive.Driver):
    """Keep the experiment PTY replay, replacing only its FD3 transport."""
    def __init__(self, shell, prefix, hook, temp):
        super().__init__(shell, prefix)
        self.hook, self.temp = hook, temp
        self.sock = self.hook_in = self.hook_out = self.proc = None
        self.forwarder = None

    def spawn(self):
        rx, tx = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.sock = rx
        stream_r, stream_w = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        self.hook_in = subprocess.PIPE
        self.proc = subprocess.Popen([str(self.hook),
            "--adapter=" + self.shell, "--session=matrix-" + self.shell],
            stdin=subprocess.PIPE, stdout=stream_w, stderr=subprocess.PIPE,
            pass_fds=(stream_w.fileno(),))
        stream_w.close()
        self.hook_out = stream_r
        self.lines = []
        self.forwarder = threading.Thread(target=self._forward, args=(tx,), daemon=True)
        self.forwarder.start()
        argv = drive.shell_argv(self.shell)
        script = ROOT / "shell" / self.shell / {"bash":"bash.sh","zsh":"zsh.zsh","fish":"fish.fish"}[self.shell]
        if self.shell == "bash": argv = ["/usr/bin/bash", "--noprofile", "--norc", "-i"]
        elif self.shell == "zsh": argv = [str(ROOT / "experiments/toolchain/root/usr/bin/zsh"), "-f", "-i"]
        else: argv = ["/usr/bin/fish", "--no-config", "-i"]
        env = dict(os.environ, LI_SINK_FD="3", LI_SESSION_ID="matrix-"+self.shell,
                   LI_EMIT_STARTED="1", LI_EMIT_SESSION="1",
                   HISTFILE=os.devnull, TERM="dumb")
        if self.shell == "zsh":
            env["LI_ZSH_MODULE_PATH"] = str(ROOT / "experiments/toolchain/root/usr/lib/zsh/5.9.2")
            env["LI_ZSH_FPATH"] = str(ROOT / "experiments/toolchain/root/usr/share/zsh/functions/Misc")
        pid, fd = pty.fork()
        if pid == 0:
            try:
                signal.signal(signal.SIGPIPE, signal.SIG_DFL)
                tx_fd = tx.fileno()
                if rx.fileno() != tx_fd:
                    rx.close()
                os.dup2(tx_fd, 3); os.set_inheritable(3, True)
                if tx_fd != 3: tx.close()
                os.execvpe(argv[0], argv, env)
            except BaseException: os._exit(111)
        tx.close(); self.pid, self.fd = pid, fd
        self.evfd = -1
        self.term = open(self.prefix + ".term.log", "wb", buffering=0)
        self.events = open(self.prefix + ".events.jsonl", "w")
        os.set_blocking(fd, False)
        os.set_blocking(self.sock.fileno(), False)
        self.send("source " + str(script))
        self.pump(duration=1, quiet=.25)

    def _forward(self, tx):
        while True:
            try: data = self.sock.recv(65536)
            except BlockingIOError:
                if self.proc.poll() is not None: break
                time.sleep(.002); continue
            except OSError: break
            if not data: break
            try: self.proc.stdin.write(data + b"\n"); self.proc.stdin.flush()
            except (BrokenPipeError, OSError): break
        try: self.proc.stdin.close()
        except OSError: pass
        tx.close()

    def _drain_events(self):
        try: data = self.hook_out.recv(262144)
        except (BlockingIOError, OSError): return
        for raw in data.splitlines():
            try: obj = json.loads(raw)
            except ValueError: continue
            self.lines.append(obj); self.events.write(json.dumps(obj)+"\n"); self.events.flush()

    def pump(self, duration=.5, quiet=None, hard_timeout=None):
        end = time.monotonic() + (hard_timeout or duration); last = time.monotonic()
        while time.monotonic() < end:
            if quiet and time.monotonic()-last >= quiet: break
            try: ready, _, _ = select.select([self.fd, self.hook_out], [], [], .05)
            except (OSError, ValueError): break
            for fd in ready:
                if fd == self.fd:
                    try: b=os.read(fd,65536)
                    except OSError: return
                    if b: self.term.write(b); last=time.monotonic()
                else: self._drain_events(); last=time.monotonic()

    def close(self, hard=False):
        super().close(hard=True)
        try: self.sock.send(b"")
        except OSError: pass
        if self.forwarder: self.forwarder.join(timeout=1)
        try: self.sock.close()
        except OSError: pass
        if self.proc:
            try: self.proc.stdin.close()
            except OSError: pass
            try: self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired: self.proc.kill(); self.proc.wait()
        try: self.hook_out.close()
        except OSError: pass


class Matrix(unittest.TestCase):
    def test_all_cases(self):
        self.assertIsNotNone(HOOK, "pass --hook PATH")
        for case in cases.CASES:
            for shell in SHELLS:
                with self.subTest(case=case, shell=shell), tempfile.TemporaryDirectory() as td:
                    d = ProductDriver(shell, str(Path(td)/"run"), HOOK, td)
                    d.spawn()
                    try: d.replay(["%s %s" % (op, arg)
                                   for op, arg in cases.STEPS[case][shell]])
                    finally: d.close(hard=(case == "t11"))
                    events = d.lines
                    payloads = [e.get("payload", {}) for e in events]
                    expected_cap = json.loads((ROOT / "capabilities" / (shell+".json")).read_text())
                    capability = [p.get("capability_record") for p in payloads
                                  if "capability_record" in p]
                    self.assertTrue(capability, "hook omitted capability_record")
                    self.assertTrue(all(c == expected_cap for c in capability))
                    finishes = [p for e,p in zip(events,payloads) if e.get("type")=="CommandFinished"]
                    starts = [e for e in events if e.get("type")=="CommandStarted"]
                    if case == "t11":
                        # A SIGKILLed command must leave no CommandFinished. Earlier
                        # completed lines in the same case are allowed (fish also
                        # reports its own `source` line).
                        self.assertFalse(any("sleep 30" in str(p.get("command"))
                                             for p in finishes),
                                         "the killed command produced a finish event")
                        if shell != "bash": self.assertGreaterEqual(len(starts), 1)
                    if case == "t01":
                        self.assertTrue(any("t01-normal" in str(p.get("command")) for p in finishes))
                    if case == "t13":
                        found = [p.get("command") for p in finishes]
                        self.assertEqual([x for x in found if str(x).startswith("echo t13-")],
                                         ["echo t13-%02d" % i for i in range(1,11)])
                    self.assertTrue(all("command" in p for p in finishes))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--hook")
    args, rest = parser.parse_known_args()
    HOOK = Path(args.hook).resolve() if args.hook else None
    sys.argv = [sys.argv[0]] + rest
    unittest.main()
