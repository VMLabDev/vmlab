#!/usr/bin/env python3
"""Correlate the torn frames of the tearing probe with vmlab-hv's GPU trace (card #233).

    hosttrace.py TRACE GRAB_JSON [--window-ms 50]

TRACE is a gpu-trace-<pid>.txt that vmlab-hv wrote with VMLAB_HV_GPU_TRACE
set (hv/crates/vmlab-hv-display/src/trace.rs). GRAB_JSON is the output of
rfbgrab.py, whose torn examples carry 'mono_ns', the CLOCK_MONOTONIC times of
the request and the reply of the grab. Both use the same clock.

For each torn frame: the RFB send inside the grab, the publication it
served (its 'ts'), the export and console readback of that publication, and
every event from --window-ms before the publication to the end of the
readback. Then totals over the whole trace: readbacks whose second read
differed (VMLAB_HV_GPU_TRACE_REREAD=1), and dma-buf fence states that were
busy. Prints one JSON object.
"""
import argparse
import bisect
import collections
import json
import sys


def parse(path):
    events = []
    with open(path) as f:
        for line in f:
            parts = line.split()
            if len(parts) < 2 or not parts[0].isdigit():
                continue
            fields = dict(p.split('=', 1) for p in parts[2:] if '=' in p)
            events.append((int(parts[0]), parts[1], fields))
    events.sort(key=lambda e: e[0])
    return events


def fmt(event, origin):
    t, kind, fields = event
    return f'{(t - origin) / 1e6:+9.3f} ms {kind} ' + ' '.join(f'{k}={v}' for k, v in fields.items())


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('trace')
    ap.add_argument('grab')
    ap.add_argument('--window-ms', type=float, default=50)
    args = ap.parse_args()
    events = parse(args.trace)
    times = [e[0] for e in events]
    with open(args.grab) as f:
        grab = json.load(f)
    publishes = {e[2].get('ts'): i for i, e in enumerate(events) if e[1] == 'publish'}
    readbacks = {e[2].get('seq'): e for e in events if e[1] == 'readback'}

    torn = []
    for example in grab.get('examples', []):
        if 'mono_ns' not in example:
            continue
        t0, t1 = example['mono_ns']
        lo, hi = bisect.bisect_left(times, t0), bisect.bisect_right(times, t1)
        sends = [e for e in events[lo:hi] if e[1] == 'rfb_send']
        entry = {'frame': example['frame'], 'bands': example['bands'], 'rfb_sends': len(sends)}
        if not sends:
            entry['note'] = 'no rfb_send in the grab: the trace does not cover it'
            torn.append(entry)
            continue
        ts = sends[-1][2].get('ts')
        entry['publication_ts'] = ts
        p = publishes.get(ts)
        if p is None:
            entry['note'] = 'the publication is not in the trace'
            torn.append(entry)
            continue
        export = next((e for e in events[p + 1:p + 200] if e[1] == 'export'), None)
        readback = readbacks.get(export[2].get('seq')) if export else None
        entry['export'] = export[2] if export else None
        entry['readback'] = readback[2] if readback else None
        origin = events[p][0]
        end = readback[0] + int(readback[2].get('us', '0')) * 1000 if readback else origin
        lo = bisect.bisect_left(times, origin - int(args.window_ms * 1e6))
        hi = bisect.bisect_right(times, end + 1_000_000)
        entry['events'] = [fmt(e, origin) for e in events[lo:hi] if e[1] != 'rfb_send']
        torn.append(entry)

    differ = [e for e in events if e[1] == 'readback' and e[2].get('reread') not in (None, '-', '0')]
    reread = sum(1 for e in events if e[1] == 'readback' and e[2].get('reread') not in (None, '-'))
    busy = collections.Counter()
    for _, kind, fields in events:
        for key, value in fields.items():
            if value == 'busy':
                busy[f'{kind}.{key}'] += 1
    kinds = collections.Counter(kind for _, kind, _ in events)
    print(json.dumps({
        'events': len(events),
        'seconds': round((events[-1][0] - events[0][0]) / 1e9, 1) if events else 0,
        'kinds': dict(kinds),
        'readbacks_reread': reread,
        'readbacks_differing': [dict(f, t=t) for t, _, f in differ[:50]],
        'readbacks_differing_count': len(differ),
        'busy_fences': dict(busy),
        'torn': torn,
    }, indent=1))
    return 0


if __name__ == '__main__':
    sys.exit(main())
