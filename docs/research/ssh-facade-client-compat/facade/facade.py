#!/usr/bin/env python3
"""Stand-in for vmlab's SSH facade (issue #68).

Shape mirrors what #52/#60 settled:

  * SSH is terminated on the *host*, never in the guest.
  * It listens on a **Unix socket** — standing in for the lab socket that
    `labd` owns.  Clients never touch it directly; they reach it through the
    stdio `ProxyCommand` (`ssh-proxy.py`), which is a byte pipe and nothing
    more.
  * Auth is `none`.  The trust boundary is the socket, not the SSH layer.
  * It answers `session` (pty-req / shell / exec / env / window-change /
    subsystem sftp) and `direct-tcpip`.  It refuses every request that would
    need it to *open* a channel towards the client: `tcpip-forward` (-R),
    `auth-agent-req` and `x11-req`.

Everything that arrives is logged, refused or not, because the third claim on
#68 is "confirm the request set #60 settles is what clients actually ask for".
"""

import argparse
import asyncio
import fcntl
import json
import logging
import os
import pty
import signal
import socket
import struct
import sys
import termios
import time

import asyncssh

LOG = logging.getLogger("facade")
TRACE_PATH = None
_START = time.time()


def trace(kind, **fields):
    """Append one structured line to the request trace."""
    rec = {"t": round(time.time() - _START, 4), "kind": kind, **fields}
    line = json.dumps(rec)
    LOG.info("TRACE %s", line)
    if TRACE_PATH:
        with open(TRACE_PATH, "a") as fh:
            fh.write(line + "\n")


def set_winsize(fd, width, height, pixwidth=0, pixheight=0):
    fcntl.ioctl(fd, termios.TIOCSWINSZ,
                struct.pack("HHHH", height, width, pixwidth, pixheight))


def install_request_hooks():
    """Log every channel request, channel open and global request that
    arrives — answered or refused.

    #68 item 3 wants the *actual* request set clients send, so this hooks
    asyncssh's dispatch points rather than trusting the high-level callbacks,
    which only fire for the requests asyncssh chooses to surface.
    """
    import asyncssh.channel as _ch
    import asyncssh.connection as _cn
    from asyncssh.constants import (MSG_CHANNEL_OPEN, MSG_CHANNEL_REQUEST,
                                    MSG_GLOBAL_REQUEST)

    def wrap(cls, msg, kind, field):
        """Replace one entry of a class's packet-handler table."""
        orig = cls._packet_handlers[msg]

        def hooked(self, pkttype, pktid, packet):
            name, want_reply = _peek_request(packet)
            trace(kind, **{field: name, "want_reply": want_reply})
            return orig(self, pkttype, pktid, packet)

        # the table is inherited; give this class its own copy before editing
        cls._packet_handlers = dict(cls._packet_handlers)
        cls._packet_handlers[msg] = hooked

    wrap(_ch.SSHChannel, MSG_CHANNEL_REQUEST, "channel-request", "request")
    wrap(_cn.SSHServerConnection, MSG_GLOBAL_REQUEST, "global-request",
         "request")
    wrap(_cn.SSHServerConnection, MSG_CHANNEL_OPEN, "channel-open",
         "channel_type")


def _peek_request(pkt):
    """Read (request name, want_reply) without disturbing the packet."""
    saved = pkt._idx
    try:
        name = pkt.get_string().decode("ascii", "replace")
        want_reply = pkt.get_boolean()
        return name, want_reply
    except Exception as exc:  # pragma: no cover - diagnostics only
        return f"<unreadable: {exc}>", None
    finally:
        pkt._idx = saved


class Facade(asyncssh.SSHServer):
    """One connection.  Refusals live here so they are legible."""

    def __init__(self):
        self._peer = None

    def connection_made(self, conn):
        self._conn = conn
        ext = conn.get_extra_info("client_version")
        trace("connection", client_version=ext)

    def connection_lost(self, exc):
        trace("disconnect", error=str(exc) if exc else None)

    # --- auth ------------------------------------------------------------
    def begin_auth(self, username):
        # False == "this user needs no authentication", i.e. `none` succeeds.
        trace("auth_begin", username=username, result="none-accepted")
        return False

    def auth_completed(self):
        trace("auth_completed")

    # --- channels the facade answers -------------------------------------
    def connection_requested(self, dest_host, dest_port, orig_host, orig_port):
        """`direct-tcpip` — the client asks us to dial out.  Answered."""
        trace("direct-tcpip", dest=f"{dest_host}:{dest_port}",
              orig=f"{orig_host}:{orig_port}", result="accepted")
        return True

    def unix_connection_requested(self, dest_path):
        trace("direct-streamlocal", dest=dest_path, result="accepted")
        return True

    # --- requests the facade refuses --------------------------------------
    def server_requested(self, listen_host, listen_port):
        """`tcpip-forward` (ssh -R).  Refused: it needs a guest-side listener
        and a guest-initiated channel open, which agent-proto does not have."""
        trace("tcpip-forward", listen=f"{listen_host}:{listen_port}",
              result="REFUSED")
        return False

    def unix_server_requested(self, listen_path):
        trace("streamlocal-forward", listen=listen_path, result="REFUSED")
        return False


async def handle_client(process):
    """A `session` channel that reached shell/exec (sftp is handled by
    asyncssh's SFTP server via sftp_factory)."""
    chan = process.channel
    term = process.get_terminal_type()
    size = process.get_terminal_size()
    env = dict(process.env or {})

    trace("session",
          command=process.command,
          subsystem=process.subsystem,
          pty=bool(term),
          term=term,
          term_size=list(size) if term else None,
          env=env)

    if process.command:
        argv = ["/bin/bash", "-c", process.command]
    else:
        argv = ["/bin/bash", "-l"]

    child_env = dict(os.environ)
    child_env.update(env)
    if term:
        child_env["TERM"] = term

    if term:
        master, slave = pty.openpty()
        set_winsize(slave, size[0], size[1], size[2], size[3])
        proc = await asyncio.create_subprocess_exec(
            *argv, stdin=slave, stdout=slave, stderr=slave,
            env=child_env, start_new_session=True)
        os.close(slave)
        master_w = os.dup(master)
        try:
            await process.redirect(stdin=master_w, stdout=master, stderr=None)
            await proc.wait()
        finally:
            await process.stdout.drain()
            for fd in (master, master_w):
                try:
                    os.close(fd)
                except OSError:
                    pass
    else:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=child_env)
        await process.redirect(stdin=proc.stdin, stdout=proc.stdout,
                               stderr=proc.stderr)
        await proc.wait()

    trace("session_exit", status=proc.returncode)
    process.exit(proc.returncode or 0)


async def main():
    global TRACE_PATH

    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", required=True)
    ap.add_argument("--host-key", required=True)
    ap.add_argument("--log", required=True)
    ap.add_argument("--trace", required=True)
    args = ap.parse_args()

    TRACE_PATH = args.trace

    logging.basicConfig(
        filename=args.log, level=logging.DEBUG,
        format="%(asctime)s %(name)s %(levelname)s %(message)s")
    # Packet-level trace: catches every request, including ones the high-level
    # API answers without telling us.
    asyncssh.set_log_level(logging.DEBUG)
    asyncssh.set_debug_level(2)
    install_request_hooks()

    if not os.path.exists(args.host_key):
        key = asyncssh.generate_private_key("ssh-ed25519", comment="vmlab facade")
        key.write_private_key(args.host_key)
        key.write_public_key(args.host_key + ".pub")
    os.chmod(args.host_key, 0o600)

    if os.path.exists(args.socket):
        os.unlink(args.socket)

    lsock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    lsock.bind(args.socket)
    lsock.listen(16)

    await asyncssh.listen(
        sock=lsock,
        server_factory=Facade,
        server_host_keys=[args.host_key],
        process_factory=handle_client,
        sftp_factory=asyncssh.SFTPServer,
        allow_scp=True,
        # No agent forwarding, no X11: the facade never opens a channel.
        agent_forwarding=False,
        x11_forwarding=False,
        encoding=None,
    )
    os.chmod(args.socket, 0o600)
    trace("listening", socket=args.socket)
    print(f"facade listening on {args.socket}", flush=True)

    await asyncio.Event().wait()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except (KeyboardInterrupt, SystemExit):
        pass
