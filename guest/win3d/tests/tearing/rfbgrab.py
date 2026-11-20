#!/usr/bin/env python3
"""Count torn frames of the tearing probe on a VM's console (card #134).

    rfbgrab.py VNC_SOCK [--seconds N] [--frames N] [--fps 30] [--out DIR]

Connects to the RFB server on a VM's vnc.sock, asks for full, non-incremental
frames, and looks in each one for the window of Start-TearWindow.ps1: eight
solid rectangles, one colour each, that all sit at the same x position in a
frame that was composed in order. A frame is torn when a row of a rectangle
starts or ends at another column than the others. That row is from an
earlier frame.

Prints one JSON object. With --out, writes the first torn frames there as
PPM files. Each torn example carries 'mono_ns', the CLOCK_MONOTONIC times of
the request and the reply of its grab, for hosttrace.py. Exit status: 0 with no torn frame, 1 with torn frames, 2 when the
probe window was never seen. Needs numpy.
"""
import argparse
import json
import os
import socket
import subprocess
import struct
import sys
import time

import numpy as np

# Keep in step with $colours in Start-TearWindow.ps1. Values are 0xRRGGBB.
LANES = [0xFE0114, 0x01FE28, 0x1401FE, 0xFEC801, 0x01C8FE, 0xC801FE, 0xFE6432, 0x32FE96]
MIN_PIXELS = 500  # of one colour, before a rectangle counts as on screen


def read_exact(sock, n):
    buf = bytearray(n)
    view = memoryview(buf)
    got = 0
    while got < n:
        r = sock.recv_into(view[got:], n - got)
        if r == 0:
            raise ConnectionError('RFB server closed the connection')
        got += r
    return buf


class Rfb:
    def __init__(self, path):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(path)
        self.sock = s
        version = bytes(read_exact(s, 12))
        if not version.startswith(b'RFB 003.'):
            raise ConnectionError(f'not an RFB server: {version!r}')
        s.sendall(b'RFB 003.008\n')
        count = read_exact(s, 1)[0]
        if count == 0:
            raise ConnectionError('RFB server refused: ' + self._reason())
        types = bytes(read_exact(s, count))
        if 1 not in types:
            raise ConnectionError(f'RFB server offers no "None" security: {list(types)}')
        s.sendall(b'\x01')
        if struct.unpack('>I', read_exact(s, 4))[0] != 0:
            raise ConnectionError('RFB security failed: ' + self._reason())
        s.sendall(b'\x01')  # shared
        self.width, self.height = struct.unpack('>HH', read_exact(s, 4))
        read_exact(s, 16)
        read_exact(s, struct.unpack('>I', read_exact(s, 4))[0])
        # 32 bits, little endian, true colour, red at bit 16: a pixel reads as 0x00RRGGBB.
        s.sendall(struct.pack('>BxxxBBBBHHHBBBxxx', 0, 32, 24, 0, 1, 255, 255, 255, 16, 8, 0))
        s.sendall(struct.pack('>BxHi', 2, 1, 0))  # raw only
        self.frame = np.zeros((self.height, self.width), dtype='<u4')

    def _reason(self):
        n = struct.unpack('>I', read_exact(self.sock, 4))[0]
        return bytes(read_exact(self.sock, n)).decode('latin-1')

    def grab(self):
        """One full frame, as a (height, width) array of 0x00RRGGBB."""
        self.sock.sendall(struct.pack('>BBHHHH', 3, 0, 0, 0, self.width, self.height))
        while True:
            kind = read_exact(self.sock, 1)[0]
            if kind == 0:
                rects = struct.unpack('>xH', read_exact(self.sock, 3))[0]
                for _ in range(rects):
                    x, y, w, h, enc = struct.unpack('>HHHHi', read_exact(self.sock, 12))
                    if enc != 0:
                        raise ConnectionError(f'unexpected RFB encoding {enc}')
                    data = read_exact(self.sock, w * h * 4)
                    self.frame[y:y + h, x:x + w] = np.frombuffer(data, dtype='<u4').reshape(h, w)
                return self.frame
            if kind == 2:
                continue
            if kind == 3:
                n = struct.unpack('>xxxI', read_exact(self.sock, 7))[0]
                read_exact(self.sock, n)
                continue
            raise ConnectionError(f'unexpected RFB message {kind}')


def inspect(frame):
    """(seen, x, bands): whether the window is in the frame, the x position
    most rows agree on, and the torn bands as (lane, first_row, last_row, x)."""
    pixels = frame & 0xFFFFFF
    spans = []
    for lane, colour in enumerate(LANES):
        mask = pixels == colour
        rows = np.flatnonzero(mask.any(axis=1))
        if rows.size == 0 or int(mask.sum()) < MIN_PIXELS:
            continue
        sub = mask[rows]
        left = sub.argmax(axis=1)
        right = sub.shape[1] - sub[:, ::-1].argmax(axis=1)
        spans.append((lane, rows, left, right))
    if len(spans) < len(LANES):
        return len(spans) > 0, None, [('missing', len(LANES) - len(spans))] if spans else []
    lefts = np.concatenate([s[2] for s in spans])
    values, counts = np.unique(lefts, return_counts=True)
    x = int(values[counts.argmax()])
    width = int(np.bincount(np.concatenate([s[3] - s[2] for s in spans])).argmax())
    bands = []
    for lane, rows, left, right in spans:
        bad = (left != x) | (right != x + width)
        # A gap in the rows of one rectangle is a band too: its rows show something else.
        gaps = np.flatnonzero(np.diff(rows) != 1)
        for g in gaps:
            bands.append((lane, int(rows[g]) + 1, int(rows[g + 1]) - 1, -1))
        idx = np.flatnonzero(bad)
        if idx.size:
            start = prev = idx[0]
            for i in list(idx[1:]) + [None]:
                if i is None or i != prev + 1:
                    bands.append((lane, int(rows[start]), int(rows[prev]), int(left[start])))
                    start = i
                prev = i
    return True, x, bands


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('sock')
    ap.add_argument('--seconds', type=float, default=60)
    ap.add_argument('--frames', type=int, default=0, help='stop after this many frames with the window')
    ap.add_argument('--fps', type=float, default=30)
    ap.add_argument('--out', help='directory for the first torn frames (PPM)')
    ap.add_argument('--keep', type=int, default=20, help='torn frames to save')
    ap.add_argument('--on-torn', help='shell command to start at the first torn frame')
    args = ap.parse_args()

    rfb = Rfb(args.sock)
    if args.out:
        os.makedirs(args.out, exist_ok=True)
    start = time.monotonic()
    grabbed = seen = torn = changes = saved = reconnects = 0
    last_x = None
    examples = []
    while time.monotonic() - start < args.seconds and not (args.frames and seen >= args.frames):
        t0 = time.monotonic()
        t0_ns = time.monotonic_ns()
        try:
            frame = rfb.grab()
        except (ConnectionError, OSError):
            # The server ends a connection when the screen mode changes.
            reconnects += 1
            if reconnects > 20:
                raise
            time.sleep(0.5)
            rfb = Rfb(args.sock)
            continue
        grabbed += 1
        t1_ns = time.monotonic_ns()
        has, x, bands = inspect(frame)
        if has:
            seen += 1
            if x is not None and x != last_x:
                changes += 1
                last_x = x
            if bands:
                torn += 1
                if torn == 1 and args.on_torn:
                    subprocess.Popen(args.on_torn, shell=True)
                if len(examples) < 50:
                    examples.append({'frame': grabbed, 't': round(t0 - start, 3), 'x': x, 'bands': bands[:8],
                                     'mono_ns': [t0_ns, t1_ns]})
                if args.out and saved < args.keep:
                    saved += 1
                    rgb = np.dstack([(frame >> 16) & 255, (frame >> 8) & 255, frame & 255]).astype(np.uint8)
                    with open(os.path.join(args.out, f'torn-{grabbed:06d}.ppm'), 'wb') as f:
                        f.write(b'P6 %d %d 255\n' % (rfb.width, rfb.height))
                        f.write(rgb.tobytes())
        delay = 1 / args.fps - (time.monotonic() - t0)
        if delay > 0:
            time.sleep(delay)
    elapsed = time.monotonic() - start
    print(json.dumps({
        'screen': [rfb.width, rfb.height], 'seconds': round(elapsed, 1), 'grabbed': grabbed,
        'grab_fps': round(grabbed / max(elapsed, 1e-9), 1), 'frames_with_window': seen,
        'position_changes': changes, 'reconnects': reconnects, 'torn': torn, 'examples': examples,
    }))
    return 2 if seen == 0 else 1 if torn else 0


if __name__ == '__main__':
    sys.exit(main())
