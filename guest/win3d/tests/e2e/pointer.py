#!/usr/bin/env python3
"""Host half of tests/e2e/pointer.sh: watch a VM's pointer, then judge it.

  pointer.py watch --display SOCK --rfb SOCK --seconds N [--until FILE] --out events.json
      Records every pointer shape and state vmlab-hv publishes on its native
      frame protocol (display.sock, the cursor capability only: shape,
      hotspot, hotspot position, visibility) and every cursor rectangle its
      RFB server sends (vnc.sock, the "cursor with alpha" pseudo-encoding),
      for N seconds or until FILE exists.

  pointer.py check --events events.json --plan SHAPES --x0 N --y0 N --step-x N
                   --background R,G,B --shots DIR --out pointer.json [--guest guest.json]
      Exit 0 when all of these hold:
        shapes     every planned step arrived on the native protocol, in
                   order, as the right kind of shape (arrow, ibeam, sizewe,
                   hidden) with its hotspot at the planned position
        rfb        RFB carried a shape for each kind, and an empty one for hidden
        outline    the I-beam is black with a white outline (it asks to
                   invert the screen, which a host pointer cannot do)
        frame      no console capture that shows the test window has anything
                   but the background where a step put the pointer
        recording  (with --frames) frames of a `vmlab record` video of the run
                   show a pointer at the position of every step but the
                   hidden one, and none at the hidden step's position. A
                   step whose shape has its hotspot at (0,0), the arrow, is
                   not judged: vmlab's recorder hides it (docs/pointer.md)

Only the standard library and ImageMagick's `magick` (for the captures).
"""
import argparse
import hashlib
import glob
import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time

ALPHA_CURSOR = -314
HOLD = 1.0  # a state counts as a step once it has lasted this long, in seconds


def recv_exact(sock, n):
    buf = b''
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError('socket closed')
        buf += chunk
    return buf


def bbox(alpha, w, h):
    """Bounding box of the non-transparent pixels: x0, y0, x1, y1 (inclusive), or None."""
    xs = [i % w for i, a in enumerate(alpha) if a]
    ys = [i // w for i, a in enumerate(alpha) if a]
    return [min(xs), min(ys), max(xs), max(ys)] if xs else None


def describe(w, h, hot, pixels, order):
    """A shape as plain facts. order: the byte offsets of R, G, B, A in a pixel."""
    r, g, b, a = (pixels[o::4] for o in order)
    opaque = [i for i in range(w * h) if a[i] == 255]
    return {
        'width': w, 'height': h, 'hot_x': hot[0], 'hot_y': hot[1],
        'sha256': hashlib.sha256(bytes(pixels)).hexdigest()[:16],
        'bbox': bbox(a, w, h) if w and h else None,
        'black': sum(1 for i in opaque if r[i] < 16 and g[i] < 16 and b[i] < 16),
        'white': sum(1 for i in opaque if r[i] > 239 and g[i] > 239 and b[i] > 239),
        'opaque': len(opaque),
    }


def watch_native(path, events, stop, t0):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    s.sendall(struct.pack('<IHHII', 16, 1, 0, 1, 2))  # Hello: version 1, the cursor capability
    s.settimeout(0.5)
    while not stop.is_set():
        try:
            head = recv_exact(s, 8)
        except socket.timeout:
            continue
        length, kind, _flags = struct.unpack('<IHH', head)
        s.settimeout(10)
        body = recv_exact(s, length - 8)
        s.settimeout(0.5)
        t = round(time.monotonic() - t0, 3)
        if kind == 5:
            w, h, hx, hy = struct.unpack_from('<IIII', body)
            events.append({'t': t, 'src': 'native', 'event': 'shape',
                           **describe(w, h, (hx, hy), body[16:], (2, 1, 0, 3))})
        elif kind == 6:
            x, y, visible = struct.unpack_from('<iiI', body)
            events.append({'t': t, 'src': 'native', 'event': 'state', 'x': x, 'y': y, 'visible': bool(visible)})


def watch_rfb(path, events, stop, t0):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    recv_exact(s, 12)
    s.sendall(b'RFB 003.008\n')
    n = recv_exact(s, 1)[0]
    recv_exact(s, n)
    s.sendall(b'\x01')              # security: None
    recv_exact(s, 4)
    s.sendall(b'\x01')              # shared
    w, h = struct.unpack('>HH', recv_exact(s, 4))
    recv_exact(s, 16)
    recv_exact(s, struct.unpack('>I', recv_exact(s, 4))[0])
    s.sendall(struct.pack('>BBHii', 2, 0, 2, 0, ALPHA_CURSOR))  # SetEncodings: raw, cursor with alpha
    last = None
    s.settimeout(0.5)
    while not stop.is_set():
        # A request for one pixel: the server answers when the frame or the pointer changed.
        s.sendall(struct.pack('>BBHHHH', 3, 1, 0, 0, 1, 1))
        try:
            head = recv_exact(s, 4)
        except socket.timeout:
            continue
        s.settimeout(10)
        if head[0] != 0:
            raise ValueError(f'unexpected RFB message {head[0]}')
        for _ in range(struct.unpack('>H', head[2:])[0]):
            x, y, rw, rh, enc = struct.unpack('>HHHHi', recv_exact(s, 12))
            if enc == 0:
                recv_exact(s, rw * rh * 4)
            elif enc == ALPHA_CURSOR:
                recv_exact(s, 4)
                d = describe(rw, rh, (x, y), recv_exact(s, rw * rh * 4), (0, 1, 2, 3))
                if d != last:  # the server sends the shape again on every move
                    last = d
                    events.append({'t': round(time.monotonic() - t0, 3), 'src': 'rfb', 'event': 'shape', **d})
            else:
                raise ValueError(f'unexpected RFB encoding {enc}')
        s.settimeout(0.5)
        time.sleep(0.05)


def watch(args):
    events, stop, t0, errors = [], threading.Event(), time.monotonic(), []

    def guard(fn, path):
        try:
            fn(path, events, stop, t0)
        except Exception as e:  # recorded, and judged by check
            errors.append(f'{fn.__name__}: {e!r}')

    threads = [threading.Thread(target=guard, args=a, daemon=True)
               for a in ((watch_native, args.display), (watch_rfb, args.rfb))]
    for t in threads:
        t.start()
    while time.monotonic() - t0 < args.seconds and not (args.until and os.path.exists(args.until)):
        time.sleep(0.2)
    seconds = round(time.monotonic() - t0, 3)
    stop.set()
    for t in threads:
        t.join(15)
    events.sort(key=lambda e: e['t'])
    json.dump({'started_unix': time.time() - (time.monotonic() - t0), 'seconds': seconds,
               'errors': errors, 'events': events}, open(args.out, 'w'), indent=1)
    print(f'{len(events)} pointer events, {len(errors)} errors')
    return 1 if errors else 0


def kind_of(shape):
    """arrow, ibeam, sizewe or None, from where the hotspot sits in the drawn part."""
    b = shape.get('bbox')
    if not shape['width'] or not b:
        return 'hidden'
    bw, bh = b[2] - b[0] + 1, b[3] - b[1] + 1
    hx, hy = shape['hot_x'] - b[0], shape['hot_y'] - b[1]
    if hx <= 2 and hy <= 2 and bh > bw:
        return 'arrow'
    centred = abs(hx - bw / 2) <= bw / 4 + 1.5 and abs(hy - bh / 2) <= bh / 4 + 1.5
    if centred and bh > bw:
        return 'ibeam'
    if centred and bw > bh:
        return 'sizewe'
    return None


def native_steps(events, end):
    """The native timeline as held states: (kind, x, y, shape) for each state that lasted HOLD."""
    shape, state, cur, since, out = None, None, None, 0.0, []
    for e in [e for e in events if e['src'] == 'native'] + [{'t': end, 'event': 'end'}]:
        if cur is not None and e['t'] - since >= HOLD and (not out or out[-1][:3] != cur[:3]):
            out.append(cur)
        if e['event'] == 'shape':
            shape = e
        elif e['event'] == 'state':
            state = e
        if shape is not None and state is not None:
            kind = kind_of(shape) if state['visible'] else 'hidden'
            new = (kind, state['x'], state['y'], shape)
            if cur is None or new[:3] != cur[:3]:
                since = e['t']
            cur = new
    return out


def frame_check(shots, plan, bg, size=64):
    """Captures that show the test window, and those with something else where a step put the pointer."""
    seen, dirty = [], []
    for png in sorted(glob.glob(os.path.join(shots, '*.png'))):
        raw = subprocess.run(['magick', png, '-depth', '8', 'rgb:-'], check=True, capture_output=True).stdout
        w, h = map(int, subprocess.run(['magick', 'identify', '-format', '%w %h', png], check=True,
                                       capture_output=True, text=True).stdout.split())

        def is_bg(x, y):
            p = raw[(y * w + x) * 3:(y * w + x) * 3 + 3]
            return all(abs(p[i] - bg[i]) <= 3 for i in range(3))
        # The window covers the screen: sample a grid, away from the pointer row.
        grid = [(x, y) for x in range(8, w, 97) for y in range(8, h, 89)]
        if sum(is_bg(x, y) for x, y in grid) < 0.98 * len(grid):
            continue
        seen.append(os.path.basename(png))
        bad = sum(1 for (px, py) in plan for y in range(max(0, py - size), min(h, py + size))
                  for x in range(max(0, px - size), min(w, px + size)) if not is_bg(x, y))
        if bad:
            dirty.append({'png': os.path.basename(png), 'pixels': bad})
    return seen, dirty


def recording_check(frames, plan, positions, bg, size=40):
    """For each step, the number of video frames showing the test window with a pointer at its position."""
    counts = [0] * len(plan)
    for png in sorted(glob.glob(os.path.join(frames, '*.png'))):
        raw = subprocess.run(['magick', png, '-depth', '8', 'rgb:-'], check=True, capture_output=True).stdout
        w, h = map(int, subprocess.run(['magick', 'identify', '-format', '%w %h', png], check=True,
                                       capture_output=True, text=True).stdout.split())

        def off(x, y):  # how far from the background, summed over the channels (video compression moves it a little)
            p = raw[(y * w + x) * 3:(y * w + x) * 3 + 3]
            return sum(abs(p[i] - bg[i]) for i in range(3))
        grid = [(x, y) for x in range(8, w, 97) for y in range(8, h, 89)]
        if sum(off(x, y) <= 24 for x, y in grid) < 0.98 * len(grid):
            continue
        for i, (px, py) in enumerate(positions):
            n = sum(1 for y in range(max(0, py - size), min(h, py + size))
                    for x in range(max(0, px - size), min(w, px + size)) if off(x, y) > 150)
            counts[i] += n >= 8
    return counts


def check(args):
    data = json.load(open(args.events))
    events = data['events']
    plan = [s.strip() for s in args.plan.split(',') if s.strip()]
    positions = [(args.x0 + i * args.step_x, args.y0) for i in range(len(plan))]
    bg = [int(v) for v in args.background.split(',')]
    steps = native_steps(events, data['seconds'])
    results = {}

    # shapes: the plan is a subsequence of the held states, kind and position both.
    i, matched = 0, []
    for kind, x, y, shape in steps:
        if i < len(plan) and kind == plan[i] and (kind == 'hidden' or (abs(x - positions[i][0]) <= 1 and abs(y - positions[i][1]) <= 1)):
            matched.append({'step': i, 'kind': kind, 'x': x, 'y': y, 'hot': [shape['hot_x'], shape['hot_y']],
                            'size': [shape['width'], shape['height']], 'sha256': shape['sha256']})
            i += 1
    results['shapes'] = {
        'pass': i == len(plan), 'matched': matched, 'planned': [[k, *p] for k, p in zip(plan, positions)],
        'held': [{'kind': k, 'x': x, 'y': y, 'hot': [s['hot_x'], s['hot_y']], 'bbox': s['bbox']} for k, x, y, s in steps],
    }

    rfb = [e for e in events if e['src'] == 'rfb']
    rfb_kinds = sorted({k for k in (kind_of(e) for e in rfb) if k})
    results['rfb'] = {'pass': set(plan) <= set(rfb_kinds), 'kinds': rfb_kinds, 'shapes': len(rfb)}

    ibeams = [e for e in events if e['event'] == 'shape' and e['src'] == 'native' and kind_of(e) == 'ibeam']
    results['outline'] = {
        'pass': bool(ibeams) and all(e['black'] > 0 and e['white'] > e['black'] and e['black'] + e['white'] == e['opaque'] for e in ibeams),
        'ibeams': [{k: e[k] for k in ('black', 'white', 'opaque', 'bbox')} for e in ibeams[:3]],
    }

    seen, dirty = frame_check(args.shots, positions, bg)
    results['frame'] = {'pass': len(seen) >= 3 and not dirty, 'captures_with_window': len(seen), 'with_pointer': dirty}

    counts = recording_check(args.frames, plan, positions, bg) if args.frames else []
    results['recording'] = {
        'pass': len(counts) == len(plan) and any(kind != 'hidden' for kind in plan)
        and all((n == 0) if kind == 'hidden' else (n > 0) for kind, n in zip(plan, counts)),
        'frames_with_pointer_at_step': counts,
    }

    out = {'pass': all(r['pass'] for r in results.values()) and not data['errors'], 'watch_errors': data['errors'], **results}
    if args.guest:
        try:
            out['guest'] = json.load(open(args.guest, encoding='utf-8-sig'))
        except (OSError, ValueError) as e:
            out['guest'] = f'unreadable: {e}'
    json.dump(out, open(args.out, 'w'), indent=1)
    for name, r in results.items():
        print(f"{'PASS' if r['pass'] else 'FAIL'}  pointer {name}")
    for e in data['errors']:
        print(f'FAIL  watch: {e}')
    return 0 if out['pass'] else 1


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='cmd', required=True)
    w = sub.add_parser('watch')
    w.add_argument('--display', required=True)
    w.add_argument('--rfb', required=True)
    w.add_argument('--seconds', type=float, required=True)
    w.add_argument('--until')
    w.add_argument('--out', required=True)
    c = sub.add_parser('check')
    c.add_argument('--events', required=True)
    c.add_argument('--plan', required=True)
    c.add_argument('--x0', type=int, required=True)
    c.add_argument('--y0', type=int, required=True)
    c.add_argument('--step-x', type=int, required=True)
    c.add_argument('--background', required=True)
    c.add_argument('--shots', required=True)
    c.add_argument('--out', required=True)
    c.add_argument('--guest')
    c.add_argument('--frames')
    args = ap.parse_args()
    sys.exit(watch(args) if args.cmd == 'watch' else check(args))


if __name__ == '__main__':
    main()
