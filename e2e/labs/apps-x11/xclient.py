"""A minimal X11 client with no libraries: it opens one window titled after
its first argument and keeps it mapped until killed, or exits at once with
the code in its second argument. The image carries Python and no X client,
so this is the guest program `vmlab app run --via x11` shows."""

import os
import socket
import struct
import sys
import time


def cookie(display: str) -> tuple[bytes, bytes]:
    """The MIT-MAGIC-COOKIE-1 for `display` from $XAUTHORITY, if any."""
    path = os.environ.get("XAUTHORITY")
    if not path or not os.path.exists(path):
        return b"", b""
    data = open(path, "rb").read()
    number = display.split(":")[1].split(".")[0].encode()
    off = 0

    def field():
        nonlocal off
        (n,) = struct.unpack_from(">H", data, off + 0)
        value = data[off + 2 : off + 2 + n]
        off += 2 + n
        return value

    while off + 2 <= len(data):
        off += 2  # family
        field()  # address
        num, name, value = field(), field(), field()
        if num == number and name == b"MIT-MAGIC-COOKIE-1":
            return name, value
    return b"", b""


def pad(b: bytes) -> bytes:
    return b + b"\0" * (-len(b) % 4)


def recv(sock: socket.socket, n: int) -> bytes:
    out = b""
    while len(out) < n:
        chunk = sock.recv(n - len(out))
        if not chunk:
            raise SystemExit("the X server closed the connection")
        out += chunk
    return out


def main() -> None:
    title = sys.argv[1].encode()
    if len(sys.argv) > 2:
        raise SystemExit(int(sys.argv[2]))
    display = os.environ["DISPLAY"]
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect("/tmp/.X11-unix/X" + display.split(":")[1].split(".")[0])
    name, value = cookie(display)
    sock.sendall(struct.pack("<cxHHHHxx", b"l", 11, 0, len(name), len(value)) + pad(name) + pad(value))
    head = recv(sock, 8)
    body = recv(sock, struct.unpack_from("<H", head, 6)[0] * 4)
    if head[0] != 1:
        raise SystemExit("the X server refused the connection: " + body.decode(errors="replace"))
    id_base = struct.unpack_from("<I", body, 4)[0]
    vendor_len, = struct.unpack_from("<H", body, 16)
    formats = body[21]
    screen = 32 + len(pad(b"x" * vendor_len)) + 8 * formats
    root, = struct.unpack_from("<I", body, screen)
    white, = struct.unpack_from("<I", body, screen + 8)
    wid = id_base | 1
    # CreateWindow: 200x100 at 0,0, InputOutput, background pixel white.
    sock.sendall(struct.pack("<BBHIIhhHHHHIII", 1, 0, 9, wid, root, 0, 0, 200, 100, 0, 1, 0, 0x2, white))
    # ChangeProperty WM_NAME (39), type STRING (31), format 8, replace.
    sock.sendall(struct.pack("<BBHIIIBxxxI", 18, 0, 6 + len(pad(title)) // 4, wid, 39, 31, 8, len(title)) + pad(title))
    # MapWindow.
    sock.sendall(struct.pack("<BxHI", 8, 2, wid))
    while True:
        time.sleep(3600)


main()
