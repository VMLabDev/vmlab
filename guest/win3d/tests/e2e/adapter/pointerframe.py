#!/usr/bin/env python3
"""Look for the pointer in a console capture of the pointer probe's window.

    pointerframe.py <capture.png> <x> <y>

The pointer probe (tests/e2e/adapter/probes/pointer_probe.cpp) fills the
display with (255,0,255). The host has just moved the pointer to x,y. Prints
one JSON line:

  background     the share of the capture that is the fill colour
  near           pixels that are not the fill colour in the box that holds a
                 pointer whose hot spot is at x,y (16 px before it, 80 after)
  elsewhere      such pixels in the rest of the capture
  bbox           the box around all of them, or null

A pointer that Windows drew into the frame shows as `near` > 0. ImageMagick
decodes the capture, as in tests/e2e/findprobe.py.
"""
import json
import subprocess
import sys


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    png, px, py = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    w, h = map(int, subprocess.run(['magick', 'identify', '-format', '%w %h', png], check=True,
                                   capture_output=True, text=True).stdout.split())
    fb = subprocess.run(['magick', png, '-depth', '8', 'RGB:-'], check=True, capture_output=True).stdout
    fill = bytes((255, 0, 255))
    near = elsewhere = 0
    x0 = y0 = None
    x1 = y1 = 0
    for y in range(h):
        row = fb[y * w * 3:(y + 1) * w * 3]
        if row == fill * w:
            continue
        for x in range(w):
            r, g, b = row[x * 3:x * 3 + 3]
            if r >= 247 and g <= 8 and b >= 247:
                continue
            if px - 16 <= x < px + 80 and py - 16 <= y < py + 80:
                near += 1
            else:
                elsewhere += 1
            x0 = x if x0 is None else min(x0, x)
            y0 = y if y0 is None else min(y0, y)
            x1, y1 = max(x1, x), max(y1, y)
    total = w * h
    print(json.dumps({'png': png, 'width': w, 'height': h, 'x': px, 'y': py,
                      'background': round((total - near - elsewhere) / total, 6), 'near': near,
                      'elsewhere': elsewhere, 'bbox': None if x0 is None else [x0, y0, x1, y1]}))


if __name__ == '__main__':
    main()
