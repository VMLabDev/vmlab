#!/usr/bin/env python3
"""Stand-in for `vmlab ssh-proxy` (issue #68).

#60: "labd terminates SSH and the proxy is a byte pipe over one
socket-returning lab command."  So this is exactly that and nothing else — it
connects to the lab socket and shovels bytes to and from stdio.  It never
parses SSH, never authenticates, never listens.

It records each invocation so we can prove a client really did run the
`ProxyCommand` rather than dialling a port behind our back.
"""

import os
import selectors
import socket
import sys
import time

import argparse

_ap = argparse.ArgumentParser()
_ap.add_argument("--socket", required=True)
_ap.add_argument("--log")
_ap.add_argument("rest", nargs="*")
_args = _ap.parse_args()

SOCK = _args.socket
INVOKE_LOG = _args.log


def note(msg):
    if INVOKE_LOG:
        with open(INVOKE_LOG, "a") as fh:
            fh.write(f"{time.time():.3f} pid={os.getpid()} ppid={os.getppid()} "
                     f"argv={sys.argv[1:]} {msg}\n")


def main():
    note(f"invoked (parent={_parent_cmdline()})")
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.connect(SOCK)
    except OSError as exc:
        note(f"connect failed: {exc}")
        sys.stderr.write(f"vmlab: cannot reach lab socket {SOCK}: {exc}\n")
        return 1

    stdin = sys.stdin.buffer.fileno()
    stdout = sys.stdout.buffer.fileno()
    sel = selectors.DefaultSelector()
    sel.register(stdin, selectors.EVENT_READ, "stdin")
    sel.register(s, selectors.EVENT_READ, "sock")

    up = down = 0
    while True:
        for key, _ in sel.select():
            if key.data == "stdin":
                data = os.read(stdin, 65536)
                if not data:
                    s.shutdown(socket.SHUT_WR)
                    sel.unregister(stdin)
                    continue
                s.sendall(data)
                up += len(data)
            else:
                data = s.recv(65536)
                if not data:
                    note(f"closed by facade (up={up} down={down})")
                    return 0
                os.write(stdout, data)
                down += len(data)


def _parent_cmdline():
    try:
        with open(f"/proc/{os.getppid()}/cmdline", "rb") as fh:
            return fh.read().replace(b"\0", b" ").decode(errors="replace").strip()
    except OSError:
        return "?"


if __name__ == "__main__":
    sys.exit(main())
