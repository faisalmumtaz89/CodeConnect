"""`codeconnect attach` in a real terminal: bytes, queries, resize, re-attach, exit."""

import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

# What this fake terminal answers about itself, as Warp does.
FOREGROUND = b"rgb:e4e4/eeee/f5f5"
BACKGROUND = b"rgb:1d1d/2020/2222"
ANSWERS = [
    (b"\x1b]10;?\x1b\\", b"\x1b]10;" + FOREGROUND + b"\x1b\\"),
    (b"\x1b]11;?\x1b\\", b"\x1b]11;" + BACKGROUND + b"\x1b\\"),
    (b"\x1b[6n", b"\x1b[1;1R"),
    (b"\x1b[?u", b"\x1b[?0u"),
    (b"\x1b[c", b"\x1b[?62c"),
]

# What the pane asks once it starts: Codex's start-up questions, then Claude's.
PANE_QUERIES = b"\x1b[6n\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?u\x1b[c\x1b[>0q\x1b[?u\x1b[c"

# Keys, as a terminal sends them: tmux's prefix keys, UTF-8, a bracketed paste,
# Shift+Enter in both extended encodings, Ctrl-C, arrows, and bytes a shell
# would never see from a line-buffered terminal.
KEYS = (
    b"\x02X\x01Y\x02dEND"
    + "é中😀".encode()
    + b"\x1b[200~line one\nline two \xc3\xa9\x1b[201~"
    + b"\x1b[13;2u\x1b[27;2;13~\x03\x1b[A\x1bOB\x7f\t\x00"
)


class Terminal:
    """A pseudo-terminal that answers queries the way a real terminal does."""

    def __init__(self, argv, env, size=(24, 80)):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvpe(argv[0], argv, env)
        self.resize(*size)
        self.output = bytearray()
        self.answered = 0
        self.status = None

    def resize(self, rows, cols):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(self, seconds=0.02):
        if select.select([self.fd], [], [], seconds)[0]:
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                data = b""
            if not data:
                return False
            self.output.extend(data)
        # Answer every query not yet answered, in the order they were asked.
        while True:
            found = [(self.output.find(q, self.answered), q, a) for q, a in ANSWERS]
            found = [f for f in found if f[0] >= 0]
            if not found:
                break
            at, query, answer = min(found)
            os.write(self.fd, answer)
            self.answered = at + len(query)
        return True

    def until(self, predicate, what, seconds=8):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if predicate():
                return
            if not self.pump():
                # The terminal closed; the process may still be exiting.
                time.sleep(0.02)
        assert predicate(), f"{what} timed out; output={bytes(self.output)[-2000:]!r}"

    def settle(self, seconds=0.5):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.pump()

    def exited(self):
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.waitstatus_to_exitcode(status)
        return self.status is not None

    def close(self):
        os.close(self.fd)
        end = time.monotonic() + 3
        while time.monotonic() < end:
            if self.exited():
                return
            time.sleep(0.01)
        os.kill(self.pid, signal.SIGKILL)
        os.waitpid(self.pid, 0)


codeconnect, tmux, directory = sys.argv[1:]


def check(root):
    socket = root / "s"
    env = dict(os.environ, TERM="xterm-256color", CODECONNECT_HOME=str(root / "home"))
    for name in ["TMUX", "TMUX_PANE", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"]:
        env.pop(name, None)

    def tmux_run(*args):
        return subprocess.run(
            [tmux, "-S", str(socket), *args], env=env,
            capture_output=True, check=True, timeout=3,
        ).stdout.strip()

    recorder = root / "recorder.py"
    recorder.write_text(
        "import os, pathlib, sys, time, tty\n"
        "tty.setraw(0)\n"
        "received, go, queries = sys.argv[1], pathlib.Path(sys.argv[2]), bytes.fromhex(sys.argv[3])\n"
        "os.write(1, b'PANE READY\\r\\n')\n"
        "while not go.exists(): time.sleep(0.01)\n"
        "os.write(1, queries)\n"
        "with open(received, 'ab', buffering=0) as out:\n"
        " pathlib.Path(received + '.ready').touch()\n"
        " while True:\n"
        "  data = os.read(0, 4096)\n"
        "  if not data: break\n"
        "  out.write(data)\n"
    )
    shim = root / "tmux"
    shim.write_text(
        f"#!{sys.executable}\nimport os, sys\n"
        "args = sys.argv[1:]\n"
        "at = args.index('-L')\n"
        "assert args[at + 1] == 'codeconnect', args\n"
        f"args[at:at + 2] = ['-S', {str(socket)!r}]\n"
        f"os.execv({tmux!r}, [{tmux!r}] + args)\n"
    )
    shim.chmod(0o700)
    env["CODECONNECT_TMUX"] = str(shim)
    terminals = []

    def open_terminal(argv, size=(24, 80)):
        terminal = Terminal(argv, env, size)
        terminals.append(terminal)
        return terminal

    try:
        # The reference: the same keys typed straight into the program.
        native_bytes = root / "native"
        native = open_terminal([sys.executable, str(recorder), str(native_bytes), str(root / "native-go"), ""])
        (root / "native-go").touch()
        native.until(lambda: Path(str(native_bytes) + ".ready").exists(), "native start")
        os.write(native.fd, KEYS)
        native.until(lambda: native_bytes.read_bytes() == KEYS, "native keys")
        native.close()
        terminals.remove(native)

        received = root / "received"
        go = root / "go"
        tmux_run("-f", "/dev/null", "new-session", "-d", "-s", "cc-1", "-x", "80", "-y", "24",
                 sys.executable, str(recorder), str(received), str(go), PANE_QUERIES.hex())
        tmux_run("new-session", "-d", "-s", "cc-12", "sleep", "60")
        tmux_run("set-option", "-g", "prefix", "C-b")
        tmux_run("set-option", "-g", "prefix2", "C-a")

        # First view: attach before the program asks anything, as Codex does.
        first = open_terminal([codeconnect, "attach", "cc-1"])
        first.until(lambda: b"PANE READY" in first.output, "the pane painted")
        assert tmux_run("list-clients", "-t", "=cc-12") == b"", "attached a prefix match"
        handshake = len(first.output)
        go.touch()
        first.until(lambda: Path(str(received) + ".ready").exists(), "the pane started")
        first.settle(1.0)
        asked = bytes(first.output[handshake:])
        answers = received.read_bytes()
        print("pane asked the terminal:", asked)
        print("pane was answered:", answers)
        # Only the kitty query, which tmux leaves unanswered, reached the terminal.
        assert asked.count(b"\x1b[?u") == 2, asked
        for query in [b"\x1b[c", b"\x1b[6n", b"\x1b]10;?", b"\x1b]11;?", b"\x1b[>0q"]:
            assert query not in asked, (query, asked)
        # Each query was answered exactly once, and the colours are the terminal's.
        expected = [
            rb"\x1b\[\d+;\d+R",
            rb"\x1b\]10;" + re.escape(FOREGROUND) + rb"\x1b\\",
            rb"\x1b\]11;" + re.escape(BACKGROUND) + rb"\x1b\\",
            rb"\x1b\[\?0u",
            rb"\x1b\[\?0u",
            rb"\x1b\[\?1;2;4c",
            rb"\x1b\[\?1;2;4c",
            rb"\x1bP>\|tmux [^\x1b]*\x1b\\",
        ]
        rest = answers
        for pattern in expected:
            match = re.search(pattern, rest)
            assert match, (pattern, answers)
            rest = rest[:match.start()] + rest[match.end():]
        assert rest == b"", ("unexpected input reached the pane", rest)

        # Keys arrive byte for byte, prefix keys included.
        before = received.read_bytes()
        os.write(first.fd, KEYS)
        first.until(lambda: received.read_bytes() == before + KEYS, "keys reached the pane")
        assert tmux_run("list-clients", "-t", "=cc-1"), "a prefix key detached the client"

        # A resize reaches the pane.
        first.resize(30, 100)
        os.kill(first.pid, signal.SIGWINCH)
        first.until(
            lambda: tmux_run("display-message", "-p", "-t", "=cc-1:", "#{pane_width}x#{pane_height}") == b"100x30",
            "the pane resized",
        )

        # The phone's client, attached beside this one, sees the pane and never sizes it.
        phone = subprocess.Popen(
            [tmux, "-S", str(socket), "-N", "-C", "attach-session", "-E", "-f", "ignore-size", "-t", "=cc-1"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        )
        end = time.monotonic() + 3
        while len(tmux_run("list-clients", "-t", "=cc-1").splitlines()) < 2 and time.monotonic() < end:
            time.sleep(0.02)
        assert len(tmux_run("list-clients", "-t", "=cc-1").splitlines()) == 2, "the phone did not attach"
        os.write(first.fd, b"phone")
        first.until(lambda: received.read_bytes().endswith(b"phone"), "typing beside the phone")
        assert tmux_run("display-message", "-p", "-t", "=cc-1:", "#{pane_width}x#{pane_height}") == b"100x30"
        phone.stdin.close()
        phone.wait(timeout=3)

        # Closing the terminal leaves the session running with no client.
        first.close()
        terminals.remove(first)
        tmux_run("has-session", "-t", "=cc-1")
        end = time.monotonic() + 3
        while tmux_run("list-clients", "-t", "=cc-1") and time.monotonic() < end:
            time.sleep(0.02)
        assert tmux_run("list-clients", "-t", "=cc-1") == b"", "the client outlived its terminal"

        # Re-attach: the screen is painted and no earlier query is asked again.
        answered = received.read_bytes()
        again = open_terminal([codeconnect, "attach", "cc-1"], size=(30, 100))
        again.until(lambda: b"PANE READY" in again.output, "the re-attach painted")
        again.settle(1.0)
        print("re-attach wrote:", bytes(again.output))
        assert b"\x1b[?u" not in again.output, "a query was replayed"
        assert received.read_bytes() == answered, "the re-attach typed into the pane"
        os.write(again.fd, b"back")
        again.until(lambda: received.read_bytes() == answered + b"back", "typing after re-attach")

        # The session ending ends the client cleanly and restores the terminal.
        tmux_run("kill-session", "-t", "=cc-1")
        again.until(again.exited, "the client exited", seconds=5)
        assert again.status == 0, again.status
        assert again.output.endswith(b"\x1b[<99u") or b"\x1b[?25h" in again.output[-200:], again.output[-200:]
        print("Native bytes, one answer per query, resize, phone beside, detach, re-attach and exit passed")
    finally:
        for terminal in terminals:
            terminal.close()


check(Path(directory))
