"""Drive a TUI in a pseudo-terminal and read its screen through pyte."""
import fcntl
import os
import pty
import select
import signal
import struct
import termios
import threading
import time

import pyte

KEYS = {
    "enter": b"\r", "esc": b"\x1b", "tab": b"\t", "backspace": b"\x7f",
    "up": b"\x1b[A", "down": b"\x1b[B", "right": b"\x1b[C", "left": b"\x1b[D",
    "home": b"\x1b[H", "end": b"\x1b[F",
}


class Session:
    def __init__(self, argv, env, cols, rows):
        self.cols, self.rows = cols, rows
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.lock = threading.Lock()
        self.frames = []
        self.recording = False
        self.closed = False
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                os.execve(argv[0], argv, env)
            finally:
                os._exit(127)
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self._record, daemon=True).start()
        time.sleep(0.5)

    def _read(self):
        while not self.closed:
            try:
                ready, _, _ = select.select([self.fd], [], [], 0.1)
                if ready:
                    data = os.read(self.fd, 65536)
                    if not data:
                        return
                    with self.lock:
                        self.stream.feed(data)
            except OSError:
                return

    def _record(self):
        while not self.closed:
            if self.recording:
                cells = self.grid()
                if not self.frames or cells != self.frames[-1]["cells"]:
                    self.frames.append({"t": time.time(), "cells": cells})
            time.sleep(0.04)

    def text(self):
        with self.lock:
            return "\n".join(self.screen.display)

    def grid(self):
        """Cells as [char, fg, bg, bold, reverse] rows, colors as pyte reports them."""
        with self.lock:
            return [[[c.data, c.fg, c.bg, c.bold, c.reverse] for c in
                     (self.screen.buffer[y][x] for x in range(self.cols))]
                    for y in range(self.rows)]

    def key(self, name, pause=0.25):
        os.write(self.fd, KEYS.get(name, name.encode()))
        time.sleep(pause)

    def wait(self, seconds):
        time.sleep(seconds)

    def until(self, needle, timeout=30):
        deadline = time.time() + timeout
        while needle not in self.text():
            if time.time() > deadline:
                raise TimeoutError(f"screen never showed {needle!r}:\n{self.text()}")
            time.sleep(0.1)

    def ensure(self, needle, key):
        """Press `key` unless `needle` is already on screen (toggled state)."""
        if needle not in self.text():
            self.key(key, pause=0.4)

    def record(self):
        self.frames = []
        self.recording = True

    def stop_recording(self):
        self.recording = False
        time.sleep(0.1)
        self.frames.append({"t": time.time(), "cells": self.grid()})
        return self.frames

    def close(self):
        try:
            os.write(self.fd, b"q")
        except OSError:
            pass
        deadline = time.time() + 5
        while time.time() < deadline:
            pid, _ = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                break
            time.sleep(0.1)
        else:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
        self.closed = True
        os.close(self.fd)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
