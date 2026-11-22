#!/usr/bin/env python3
"""Measure interaction latency through a VM's console (card #447).

    latency.py <vnc.sock | host:port> <out.json> [--moves N] [--stop FILE]
               [--rect X,Y,W,H] [--timeout-ms N] [--wait SECONDS]

The latency probe (tests/e2e/adapter/probes/latency_probe.cpp) fills its
window with the colour of the column the pointer is in. This RFB client does
what a person at a viewer does: it sends a pointer event that moves the
pointer to another column, and then asks the console for a few pixels of the
window again and again until they have the new colour. The time between the
pointer event and that answer is one sample. It includes the input path into
the guest, the guest's drawing and composition, the scanout, and the console's
copy of the frame.

The console answers a request at most every `poll_ms` (measured here, with
the picture at rest): a sample is late by up to that much, and by half of it
on average.

The result is one JSON object in <out.json>; tests/e2e/adapter/judge.py
decides.
"""
import argparse
import json
import os
import random
import struct
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rfbgrab import PALETTE, Rfb, level  # noqa: E402

# The probe has the same table, in this order.
COLUMN_COLOURS = ['red', 'green', 'blue', 'yellow', 'magenta', 'cyan', 'white', 'grey']
SAMPLE = 8  # the side of the square of pixels read for each answer


def colour_of(fb):
    """The palette colour all sampled pixels agree on, or None."""
    seen = set()
    for o in range(0, len(fb), 4):
        seen.add((level(fb[o + 2]), level(fb[o + 1]), level(fb[o])))
    return PALETTE.get(seen.pop()) if len(seen) == 1 else None


def percentile(sorted_values, p):
    return sorted_values[min(len(sorted_values) - 1, int(len(sorted_values) * p))]


def summary(values):
    v = sorted(values)
    return {'n': len(v), 'mean': round(sum(v) / len(v), 2), 'p50': round(percentile(v, 0.5), 2),
            'p90': round(percentile(v, 0.9), 2), 'p99': round(percentile(v, 0.99), 2), 'max': round(v[-1], 2)}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('target')
    ap.add_argument('out')
    ap.add_argument('--moves', type=int, default=120)
    ap.add_argument('--stop')
    ap.add_argument('--rect', default='32,32,800,480')
    ap.add_argument('--timeout-ms', type=float, default=2000)
    ap.add_argument('--wait', type=float, default=120, help='seconds to wait for the window')
    a = ap.parse_args()
    x, y, w, h = map(int, a.rect.split(','))
    res = {'target': a.target, 'rect': [x, y, w, h], 'moves': 0, 'timeouts': 0, 'timeout_ms': a.timeout_ms,
           'samples_ms': [], 'error': None}
    start = time.monotonic()
    try:
        rfb = Rfb(a.target)
        res['console'] = [rfb.width, rfb.height]
        if x + w > rfb.width or y + h > rfb.height:
            raise RuntimeError(f'the window rectangle does not fit the {rfb.width}x{rfb.height} console')
        fb = bytearray(SAMPLE * SAMPLE * 4)
        # Read below the pointer's row, for a console that paints the pointer into the frame.
        sx, sy = x + w // 2, y + h * 3 // 4
        py = y + h // 4

        def look():
            rfb.frame(sx, sy, SAMPLE, SAMPLE, fb)
            return colour_of(fb)

        def point(column):
            px = x + column * w // 8 + w // 16
            rfb.s.sendall(struct.pack('>BBHH', 5, 0, px, py))

        deadline = time.monotonic() + a.wait
        while look() is None:
            if time.monotonic() > deadline or (a.stop and os.path.exists(a.stop)):
                raise RuntimeError('the probe window did not appear on the console')
            time.sleep(0.5)
        # How often the console answers while nothing changes.
        t0 = time.monotonic()
        for _ in range(50):
            look()
        res['poll_ms'] = round((time.monotonic() - t0) * 1000 / 50, 2)

        rng = random.Random(447)
        column = 0
        point(column)
        time.sleep(1)
        for _ in range(a.moves):
            if a.stop and os.path.exists(a.stop):
                break
            column = (column + rng.choice((2, 3, 5))) % 8
            want = COLUMN_COLOURS[column]
            # Not in step with the display: wait a random part of a few frames.
            time.sleep(rng.uniform(0.15, 0.35))
            if look() == want:
                continue  # cannot happen unless the picture is stale; do not count it
            t0 = time.monotonic()
            point(column)
            res['moves'] += 1
            while True:
                seen = look()
                dt = (time.monotonic() - t0) * 1000
                if seen == want:
                    res['samples_ms'].append(round(dt, 2))
                    break
                if dt > a.timeout_ms:
                    res['timeouts'] += 1
                    break
        if res['samples_ms']:
            res['latency_ms'] = summary(res['samples_ms'])
    except (OSError, EOFError, RuntimeError, struct.error) as e:
        res['error'] = f'{type(e).__name__}: {e}'
    res['seconds'] = round(time.monotonic() - start, 2)
    with open(a.out, 'w') as f:
        json.dump(res, f, indent=2)
    lat = res.get('latency_ms')
    print(f"latency: {res['moves']} moves, {res['timeouts']} without an answer"
          + (f", p50 {lat['p50']} ms, p90 {lat['p90']} ms, max {lat['max']} ms, console poll {res.get('poll_ms')} ms"
             if lat else '')
          + (f"; error: {res['error']}" if res['error'] else ''))
    return 1 if res['error'] else 0


if __name__ == '__main__':
    sys.exit(main())
