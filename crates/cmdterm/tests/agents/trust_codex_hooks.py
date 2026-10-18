#!/usr/bin/env python3
"""Trusts the hooks the Codex integration installed, as a user does once.

Codex asks before it runs hooks it has not seen. This starts Codex in a
pseudo terminal in WORKDIR, answers that question with "trust all" and quits
once config.toml holds a trusted hash for every hook in hooks.json.
Usage: trust_codex_hooks.py WORKDIR
"""

import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

workdir = sys.argv[1]
home = os.environ["HOME"]
with open(os.path.join(home, ".codex/hooks.json")) as f:
    hooks = sum(len(entries) for entries in json.load(f)["hooks"].values())


def trusted():
    try:
        with open(os.path.join(home, ".codex/config.toml")) as f:
            return f.read().count("trusted_hash")
    except OSError:
        return 0


pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    os.chdir(workdir)
    os.environ["TERM"] = "xterm-256color"
    os.execvp("codex", ["codex"])

screen = b""
answered = {}
deadline = time.time() + 90
try:
    while trusted() < hooks:
        if time.time() > deadline:
            sys.exit(f"hooks still untrusted ({trusted()} of {hooks})\n"
                     + screen[-3000:].decode(errors="replace"))
        ready, _, _ = select.select([fd], [], [], 0.5)
        if ready:
            try:
                screen += os.read(fd, 65536)
            except OSError:
                sys.exit("codex quit\n" + screen[-3000:].decode(errors="replace"))
            # Answers what Codex asks the terminal: the cursor position,
            # the device attributes and the colors.
            for query, reply in ((b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?62;22c"),
                                 (b"\x1b]10;?\x1b\\", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
                                 (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
                                 (b"\x1b[?u", b"\x1b[?0u")):
                if query in screen:
                    os.write(fd, reply)
                    screen = screen.replace(query, b"")
        tail = screen[-8192:]
        for text, keys in ((b"need review", b"2\r"), (b"trust all", b"t")):
            if text in tail and time.time() - answered.get(text, 0) > 3:
                os.write(fd, keys)
                answered[text] = time.time()
finally:
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
