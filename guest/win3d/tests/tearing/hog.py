#!/usr/bin/env python3
"""Keep a set of CPUs busy for a share of the time.

    hog.py PERCENT CPU[,CPU...] [--period-ms 10]

One process per CPU, pinned to it, busy for PERCENT of every period. It is
the host load of the tearing probe (run.sh --load): together with a render
server at SCHED_IDLE it leaves the server the gaps only, as on a slow host.
Runs until it gets SIGTERM or SIGINT.
"""
import argparse
import os
import signal
import sys
import time


def spin(cpu, percent, period):
    os.sched_setaffinity(0, {cpu})
    busy = period * percent / 100
    while True:
        start = time.monotonic()
        while time.monotonic() - start < busy:
            pass
        rest = period - (time.monotonic() - start)
        if rest > 0:
            time.sleep(rest)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('percent', type=float)
    ap.add_argument('cpus')
    ap.add_argument('--period-ms', type=float, default=10)
    args = ap.parse_args()
    if not 0 < args.percent <= 100:
        sys.exit('PERCENT is above 0 and at most 100')
    children = []
    for cpu in (int(c) for c in args.cpus.split(',')):
        pid = os.fork()
        if pid == 0:
            signal.signal(signal.SIGTERM, signal.SIG_DFL)
            spin(cpu, args.percent, args.period_ms / 1000)
        children.append(pid)

    def stop(*_):
        for pid in children:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        sys.exit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    signal.pause()


if __name__ == '__main__':
    main()
