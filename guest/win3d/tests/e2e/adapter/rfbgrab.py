#!/usr/bin/env python3
"""Read the tearing probe's window from a VM's console and count torn frames.

    rfbgrab.py <vnc.sock | host:port> <out.json> [--seconds N] [--stop FILE]
               [--rect X,Y,W,H] [--save-torn FILE.png]

The tearing probe (tests/e2e/adapter/probes/tear_probe.cpp) draws every frame
of its window in one colour of an eight-colour palette. This RFB client asks
the console for the window's rectangle again and again, as full updates and
not incremental ones, about 30 times a second, and looks at each answer row by
row. A frame is

  no window   when fewer than 90 percent of its rows have a palette colour
              (the probe has not started, or has ended);
  whole       when every row has the same palette colour;
  torn        otherwise: rows of two frames, or rows that are no palette
              colour. This is what card #134 calls a stale band.

A torn frame that is the same as the torn frame before it is also counted as
`torn_persisting`: the probe holds its picture for a few seconds at a time,
and a band that stays on the screen through a hold is what a user sees.

It runs for --seconds (60), or until the file --stop names exists. The result
is one JSON object in <out.json>; it is the measurement, and
tests/e2e/adapter/judge.py decides.
"""
import argparse
import json
import os
import socket
import struct
import sys
import time
import zlib

# The probe has the same table. Levels per channel: 0, 1, or "mid" for 96.
PALETTE = {(1, 0, 0): 'red', (0, 1, 0): 'green', (0, 0, 1): 'blue', (1, 1, 0): 'yellow', (1, 0, 1): 'magenta',
           (0, 1, 1): 'cyan', (1, 1, 1): 'white', (2, 2, 2): 'grey'}
COLUMNS = 8


def level(v):
    if v >= 224:
        return 1
    if v <= 32:
        return 0
    if 72 <= v <= 120:
        return 2
    return 3  # not a palette level


class Rfb:
    def __init__(self, target):
        if ':' in target and not target.startswith('/'):
            host, port = target.rsplit(':', 1)
            self.s = socket.create_connection((host, int(port)), timeout=30)
        else:
            self.s = socket.socket(socket.AF_UNIX)
            self.s.settimeout(30)
            self.s.connect(target)
        self.rx(12)
        self.s.sendall(b'RFB 003.008\n')
        types = self.rx(self.rx(1)[0])
        if 1 not in types:
            raise RuntimeError(f'the console offers no "None" security: {list(types)}')
        self.s.sendall(b'\x01')
        if struct.unpack('>I', self.rx(4))[0]:
            raise RuntimeError('RFB security failed')
        self.s.sendall(b'\x01')  # shared: leave other viewers connected
        self.width, self.height = struct.unpack('>HH', self.rx(4))
        self.rx(16)
        self.rx(struct.unpack('>I', self.rx(4))[0])
        # 32 bits per pixel, little endian, B G R X in memory.
        self.s.sendall(struct.pack('>BxxxBBBBHHHBBBxxx', 0, 32, 24, 0, 1, 255, 255, 255, 16, 8, 0))
        # Raw only, and the cursor as its own pseudo-rectangle (-239), so that a
        # server that has the pointer's shape does not paint it into the frame.
        self.s.sendall(struct.pack('>BxHii', 2, 2, 0, -239))

    def rx(self, n):
        buf = bytearray()
        while len(buf) < n:
            chunk = self.s.recv(min(n - len(buf), 1 << 20))
            if not chunk:
                raise EOFError('the console closed the connection')
            buf += chunk
        return bytes(buf)

    def frame(self, x, y, w, h, fb, incremental=False):
        """Ask for the rectangle and copy the answer's pixels into fb (w*h*4 bytes).

        An incremental request is answered when the console has a new frame.
        """
        self.s.sendall(struct.pack('>BBHHHH', 3, 1 if incremental else 0, x, y, w, h))
        while True:
            t = self.rx(1)[0]
            if t == 0:
                self.rx(1)
                for _ in range(struct.unpack('>H', self.rx(2))[0]):
                    rx_, ry, rw, rh, enc = struct.unpack('>HHHHi', self.rx(12))
                    if enc == -239:
                        self.rx(rw * rh * 4 + ((rw + 7) // 8) * rh)
                        continue
                    if enc != 0:
                        raise RuntimeError(f'the console sent encoding {enc}; only raw was asked for')
                    data = self.rx(rw * rh * 4)
                    # Copy the part of this rectangle that lies inside ours.
                    x0, x1 = max(rx_, x), min(rx_ + rw, x + w)
                    if x0 >= x1:
                        continue
                    for row in range(max(ry, y), min(ry + rh, y + h)):
                        src = ((row - ry) * rw + (x0 - rx_)) * 4
                        dst = ((row - y) * w + (x0 - x)) * 4
                        fb[dst:dst + (x1 - x0) * 4] = data[src:src + (x1 - x0) * 4]
                return
            if t == 1:
                self.rx(3)
                self.rx(struct.unpack('>H', self.rx(2))[0] * 6)
            elif t == 2:
                pass
            elif t == 3:
                self.rx(3)
                self.rx(struct.unpack('>I', self.rx(4))[0])
            else:
                raise RuntimeError(f'unknown RFB message {t}')


def rows_of(fb, w, h):
    """The colour name of each row, or None when its samples do not agree on a palette colour."""
    cols = [int((i + 0.5) * w / COLUMNS) for i in range(COLUMNS)]
    out = []
    for row in range(h):
        base = row * w * 4
        seen = {}
        for c in cols:
            o = base + c * 4
            key = (level(fb[o + 2]), level(fb[o + 1]), level(fb[o]))
            seen[key] = seen.get(key, 0) + 1
        key, n = max(seen.items(), key=lambda kv: kv[1])
        out.append(PALETTE.get(key) if n >= COLUMNS - 1 else None)
    return out


def runs_of(rows):
    """[[colour, first row, rows], ...]"""
    runs = []
    for i, c in enumerate(rows):
        if runs and runs[-1][0] == c:
            runs[-1][2] += 1
        else:
            runs.append([c, i, 1])
    return runs


def write_png(path, fb, w, h):
    raw = bytearray()
    for r in range(h):
        raw.append(0)
        row = fb[r * w * 4:(r + 1) * w * 4]
        for i in range(0, len(row), 4):
            raw += bytes((row[i + 2], row[i + 1], row[i]))

    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))

    with open(path, 'wb') as f:
        f.write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0)) +
                chunk(b'IDAT', zlib.compress(bytes(raw), 6)) + chunk(b'IEND', b''))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('target')
    ap.add_argument('out')
    ap.add_argument('--seconds', type=float, default=60)
    ap.add_argument('--stop')
    ap.add_argument('--rect', default='32,32,800,480')
    ap.add_argument('--save-torn')
    a = ap.parse_args()
    x, y, w, h = map(int, a.rect.split(','))
    res = {'target': a.target, 'rect': [x, y, w, h], 'frames': 0, 'window_frames': 0, 'whole_frames': 0,
           'torn_frames': 0, 'torn_persisting': 0, 'colour_changes': 0, 'colours_seen': [], 'torn': [],
           'error': None}
    start = time.monotonic()
    try:
        rfb = Rfb(a.target)
        res['console'] = [rfb.width, rfb.height]
        if x + w > rfb.width or y + h > rfb.height:
            raise RuntimeError(f'the window rectangle does not fit the {rfb.width}x{rfb.height} console')
        fb = bytearray(w * h * 4)
        last_colour = last_torn = None
        colours = set()
        while time.monotonic() - start < a.seconds and not (a.stop and os.path.exists(a.stop)):
            t0 = time.monotonic()
            rfb.frame(x, y, w, h, fb)
            res['frames'] += 1
            rows = rows_of(fb, w, h)
            named = [c for c in rows if c]
            if len(named) < 0.9 * h:
                last_torn = None
                continue
            res['window_frames'] += 1
            runs = runs_of(rows)
            if len(runs) == 1:
                res['whole_frames'] += 1
                colours.add(runs[0][0])
                if last_colour is not None and runs[0][0] != last_colour:
                    res['colour_changes'] += 1
                last_colour = runs[0][0]
                last_torn = None
            else:
                res['torn_frames'] += 1
                if runs == last_torn:
                    res['torn_persisting'] += 1
                last_torn = runs
                if len(res['torn']) < 20:
                    res['torn'].append({'t': round(t0 - start, 2), 'frame': res['frames'], 'runs': runs})
                if a.save_torn and res['torn_frames'] == 1:
                    write_png(a.save_torn, fb, w, h)
            spare = 1 / 30 - (time.monotonic() - t0)
            if spare > 0:
                time.sleep(spare)
        res['colours_seen'] = sorted(colours)
    except (OSError, EOFError, RuntimeError, struct.error) as e:
        res['error'] = f'{type(e).__name__}: {e}'
    res['seconds'] = round(time.monotonic() - start, 2)
    res['fps'] = round(res['frames'] / res['seconds'], 1) if res['seconds'] > 0 else 0
    with open(a.out, 'w') as f:
        json.dump(res, f, indent=2)
    print(f"rfbgrab: {res['frames']} frames in {res['seconds']} s, {res['window_frames']} with the window, "
          f"{res['torn_frames']} torn ({res['torn_persisting']} persisting)"
          + (f"; error: {res['error']}" if res['error'] else ''))
    return 1 if res['error'] else 0


if __name__ == '__main__':
    sys.exit(main())
