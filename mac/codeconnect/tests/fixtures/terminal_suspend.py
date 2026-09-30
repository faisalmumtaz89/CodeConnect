"""Ctrl+Z through `codeconnect attach`, in a real terminal running an interactive shell.

The pane runs a stand-in agent under `codeconnect internal-job` that stops itself on
Ctrl+Z as Claude and Codex do. What is checked is what the shell and the processes do:
the viewer stops as the shell's job, `fg` continues the agent, a marker the agent merely
prints stops nothing, two markers reaching the viewer before tmux answers about the first
stop it once, and a session that ends while the viewer is stopped lets `fg` end it
quietly. Text is typed into the shell only while the shell owns the terminal.
"""

import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

codeconnect, tmux, directory = sys.argv[1:]
root = Path(directory)
socket = root / "s"
env = dict(os.environ, TERM="xterm-256color", CODECONNECT_HOME=str(root / "home"), PS1="$ ")
for name in ["TMUX", "TMUX_PANE", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"]:
    env.pop(name, None)

# `codeconnect attach` asks for tmux's `-L codeconnect`; this server is `-S`. While
# `batch` exists, the viewer's control client holds back what tmux says from one stop
# marker until the next, so the viewer reads both before any answer to its questions;
# `batch` is then renamed `batched`.
shim = root / "tmux"
batch = root / "batch"
shim.write_text(
    f"#!{sys.executable}\nimport os, subprocess, sys, threading\n"
    "args = sys.argv[1:]\n"
    "if '-L' in args:\n"
    " at = args.index('-L')\n"
    f" args[at:at + 2] = ['-S', {str(socket)!r}]\n"
    "if '-C' not in args:\n"
    f" os.execv({tmux!r}, [{tmux!r}] + args)\n"
    f"batch = {str(batch)!r}\n"
    f"client = subprocess.Popen([{tmux!r}] + args, stdin=subprocess.PIPE, stdout=subprocess.PIPE)\n"
    "def forward():\n"
    " while data := os.read(0, 4096):\n"
    "  client.stdin.write(data)\n"
    "  client.stdin.flush()\n"
    " client.stdin.close()\n"
    "threading.Thread(target=forward, daemon=True).start()\n"
    "held = []\n"
    "for line in client.stdout:\n"
    " marker = line.startswith(b'%output') and b'P=codeconnect-stopped' in line\n"
    " if held or (marker and os.path.exists(batch)):\n"
    "  held.append(line)\n"
    "  if not marker or len(held) == 1:\n"
    "   continue\n"
    "  os.rename(batch, batch + 'ed')\n"
    "  line, held = b''.join(held), []\n"
    " os.write(1, line)\n"
    "sys.exit(client.wait())\n"
)
shim.chmod(0o700)
env["CODECONNECT_TMUX"] = str(shim)

agent = root / "agent.py"
agent.write_text(
    "import os, signal, sys, termios, tty\n"
    "root = sys.argv[1]\n"
    "cooked = termios.tcgetattr(0)\n"
    "tty.setraw(0)\n"
    "os.write(1, b'AGENT READY\\r\\n')\n"
    "stops = 0\n"
    "while True:\n"
    " data = os.read(0, 64)\n"
    " if not data: break\n"
    " for byte in data:\n"
    "  if byte in (0x1a, ord('Z')):\n"
    "   termios.tcsetattr(0, termios.TCSANOW, cooked)\n"
    "   os.write(1, b'SUSPENDED\\r\\n' if byte == 0x1a else b'\\x1bP=codeconnect-stopped\\x1b\\\\')\n"
    "   os.kill(0, signal.SIGTSTP)\n"
    "   tty.setraw(0)\n"
    "   stops += 1\n"
    "   open(os.path.join(root, 'resumed-%d' % stops), 'w').close()\n"
    "  elif byte == ord('M'):\n"
    "   os.write(1, b'\\x1bP=codeconnect-stopped\\x1b\\\\')\n"
    "  else:\n"
    "   with open(os.path.join(root, 'received'), 'ab') as out: out.write(bytes([byte]))\n"
)


def tmux_run(*args):
    return subprocess.run([tmux, "-S", str(socket), *args], env=env,
                          capture_output=True, timeout=3).stdout.strip()


def state(pid):
    return subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()


class Shell:
    def __init__(self):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve("/bin/bash", ["bash", "--norc", "--noprofile", "-i"], env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        self.output = bytearray()
        self.answered = 0

    def pump(self, seconds=0.02):
        if select.select([self.fd], [], [], seconds)[0]:
            try:
                self.output.extend(os.read(self.fd, 65536))
            except OSError:
                pass
        # Answer the viewer's handshake as a terminal does.
        for query, answer in [(b"\x1b[6n", b"\x1b[5;1R"), (b"\x1b[c", b"\x1b[?62c")]:
            at = self.output.find(query, self.answered)
            while at >= 0:
                os.write(self.fd, answer)
                self.answered = at + len(query)
                at = self.output.find(query, self.answered)

    def until(self, predicate, what, seconds=10):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if predicate():
                return
            self.pump()
        assert predicate(), f"{what} timed out; output={bytes(self.output)[-1500:]!r}"

    def owns_terminal(self):
        out = subprocess.run(["ps", "-o", "tpgid=", "-p", str(self.pid)], capture_output=True, text=True).stdout
        return out.strip() == str(self.pid)

    def type(self, text):
        self.until(self.owns_terminal, f"the shell to own the terminal before typing {text!r}")
        os.write(self.fd, text.encode())

    def viewer(self):
        out = subprocess.run(["pgrep", "-P", str(self.pid)], capture_output=True, text=True).stdout.split()
        return int(out[0]) if out else None


shell = Shell()
try:
    tmux_run("-f", "/dev/null", "new-session", "-d", "-s", "cc-1", "-x", "80", "-y", "24",
             codeconnect, "internal-job", sys.executable, str(agent), str(root))
    pane_pid = int(tmux_run("display-message", "-p", "-t", "=cc-1:", "#{pane_pid}"))
    shell.until(lambda: subprocess.run(["pgrep", "-P", str(pane_pid)], capture_output=True).stdout, "the agent")
    agent_pid = int(subprocess.run(["pgrep", "-P", str(pane_pid)], capture_output=True, text=True).stdout.split()[0])
    received = root / "received"

    shell.pump(0.5)
    shell.type(f"{codeconnect} attach cc-1\r")
    shell.until(lambda: b"AGENT READY" in shell.output and not shell.owns_terminal(), "the attach")
    viewer = shell.viewer()
    os.write(shell.fd, b"a")
    shell.until(lambda: received.exists() and received.read_bytes() == b"a", "a key reaching the agent")

    # A marker the agent merely prints is not a stop.
    os.write(shell.fd, b"M")
    shell.pump(1.0)
    assert not state(viewer).startswith("T"), f"a printed marker stopped the viewer: {state(viewer)}"
    os.write(shell.fd, b"b")
    shell.until(lambda: received.read_bytes() == b"ab", "typing after a printed marker")

    for stop in (1, 2):
        os.write(shell.fd, b"\x1a")
        shell.until(lambda: state(agent_pid).startswith("T") and state(viewer).startswith("T")
                    and shell.owns_terminal(), f"stop {stop}: the agent and the viewer stopped, the shell back")
        shell.type("fg\r")
        shell.until(lambda: (root / f"resumed-{stop}").exists(), f"stop {stop}: fg continued the agent")
        shell.until(lambda: not shell.owns_terminal() and not state(viewer).startswith("T"),
                    f"stop {stop}: the viewer back in the foreground")
        os.write(shell.fd, str(stop).encode())
        shell.until(lambda: received.read_bytes().endswith(str(stop).encode()), f"stop {stop}: typing after fg")

    # The agent prints a marker and then stops: two markers reach the viewer before tmux
    # answers the question the first one raises. It stops once, and `fg` continues it.
    batch.touch()
    os.write(shell.fd, b"Z")
    shell.until(lambda: state(agent_pid).startswith("T") and state(viewer).startswith("T")
                and shell.owns_terminal(), "two markers: the agent and the viewer stopped, the shell back")
    assert (root / "batched").exists(), "the two markers did not reach the viewer together"
    shell.type("fg\r")
    shell.until(lambda: (root / "resumed-3").exists(), "two markers: fg continued the agent")
    shell.until(lambda: not shell.owns_terminal() and not state(viewer).startswith("T"),
                "two markers: the viewer back in the foreground")
    os.write(shell.fd, b"3")
    shell.until(lambda: received.read_bytes().endswith(b"3"), "two markers: typing after fg")

    # The session ends while the viewer is stopped: `fg` ends it quietly.
    os.write(shell.fd, b"\x1a")
    shell.until(lambda: state(viewer).startswith("T") and shell.owns_terminal(), "the last stop")
    tmux_run("kill-session", "-t", "=cc-1")
    mark = len(shell.output)
    shell.type("fg\r")
    shell.until(lambda: state(viewer) == "" and shell.owns_terminal(), "fg after the session ended")
    shell.pump(0.5)
    after = bytes(shell.output[mark:])
    print("after fg on an ended session:", after)
    assert b"Error" not in after and b"Broken pipe" not in after, after
    shell.type("echo status=$?\r")
    shell.until(lambda: b"status=0" in shell.output[mark:], "the viewer's exit status")
finally:
    os.close(shell.fd)
    os.kill(shell.pid, signal.SIGKILL)
    os.waitpid(shell.pid, 0)
