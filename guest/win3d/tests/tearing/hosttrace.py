#!/usr/bin/env python3
"""Correlate the torn frames of the tearing probe with vmlab-hv's GPU trace (cards #233, #237).

    hosttrace.py TRACE GRAB_JSON [--window-ms 50] [--vk VK_TRACE ...]

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

--vk adds the traces of the render workers (gpu-trace-<pid>.txt of each,
from patches/virglrenderer/diagnostic/0041): every vk_submit with the
virgl resources its command buffers use, and its vk_done. Then 'submits'
says, for each composition (a submission that writes a scanout), whether a
host present copy into a resource that it reads ran between its submission
and its completion, for all compositions and for those of the torn frames.
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


def resources(fields):
    """The resources of a vk_submit as {id: flags}."""
    out = {}
    for item in fields.get('res', '-').split(','):
        if ':' in item:
            res, flags = item.split(':', 1)
            out[res] = flags
    return out


def submits(events, vk_events, torn, alias=None):
    """Compositions against the host present copies into what they read.
    `alias` maps a present copy's destination to the resource that the
    render worker knows the same memory as."""
    alias = alias or {}
    done = {(f.get('ctx'), f.get('seq')): t for t, kind, f in vk_events if kind == 'vk_done'}
    subs = []
    for t, kind, f in vk_events:
        if kind != 'vk_submit':
            continue
        subs.append({'t': t, 'done': done.get((f.get('ctx'), f.get('seq'))), 'ctx': f.get('ctx'),
                     'name': f.get('name'), 'seq': f.get('seq'), 'res': resources(f)})
    # A present copy runs from its pcopy record to its pcopy_src record (written
    # after the copy with the trace on), or to the next command without one.
    copies = []
    open_copy = {}
    for t, kind, f in events:
        key = (f.get('src'), f.get('dst'))
        if kind == 'pcopy':
            open_copy[key] = len(copies)
            copies.append({'start': t, 'end': None, 'src': key[0], 'dst': alias.get(key[1], key[1])})
        elif kind == 'pcopy_src' and key in open_copy:
            copies[open_copy.pop(key)]['end'] = t
    copies = [c for c in copies if c['end'] is not None]
    scanouts = {f.get('res') for _, kind, f in events if kind == 'export'}
    by_dst = collections.defaultdict(list)
    for c in copies:
        by_dst[c['dst']].append(c)
    for v in by_dst.values():
        v.sort(key=lambda c: c['start'])
    starts = {d: [c['start'] for c in v] for d, v in by_dst.items()}

    def relation(sub):
        """The present copies into a resource that `sub` reads and how they lie
        against its submission (t) and completion (done)."""
        out = []
        end = sub['done'] or sub['t']
        for res, flags in sub['res'].items():
            if res in scanouts or not ({'d', 'r'} & set(flags)) or res not in by_dst:
                continue
            i = bisect.bisect_right(starts[res], end)
            for c in by_dst[res][max(0, i - 2):i + 1]:
                if c['end'] < sub['t'] - 50_000_000 or c['start'] > end + 50_000_000:
                    continue
                if c['end'] <= sub['t']:
                    where = 'copy-before-submit'
                elif sub['done'] is not None and c['start'] >= sub['done']:
                    where = 'copy-after-done'
                else:
                    where = 'copy-overlaps'
                out.append({'res': res, 'flags': flags, 'where': where,
                            'copy_start_ms': round((c['start'] - sub['t']) / 1e6, 3),
                            'copy_end_ms': round((c['end'] - sub['t']) / 1e6, 3)})
        return out

    compositions = [s for s in subs if any(r in scanouts and 'w' in f for r, f in s['res'].items())]
    counts = collections.Counter()
    by_name = collections.Counter()
    for s in compositions:
        rel = relation(s)
        kinds = {r['where'] for r in rel}
        counts['compositions'] += 1
        counts['reading-a-copied-resource'] += bool(rel)
        counts['with-an-overlapping-copy'] += 'copy-overlaps' in kinds
        by_name[s['name']] += 1
    times = [s['t'] for s in subs]
    frames = []
    for entry in torn:
        scanout = (entry.get('export') or {}).get('res')
        origin = entry.get('publication_t')
        if scanout is None or origin is None:
            continue
        i = bisect.bisect_left(times, origin)
        comp = next((subs[j] for j in range(i - 1, max(-1, i - 400), -1)
                     if 'w' in subs[j]['res'].get(scanout, '')), None)
        if comp is None:
            frames.append({'frame': entry['frame'], 'scanout': scanout, 'note': 'no submission wrote the scanout'})
            continue
        frames.append({
            'frame': entry['frame'], 'scanout': scanout, 'composition': {
                'name': comp['name'], 'ctx': comp['ctx'], 'seq': comp['seq'],
                'submit_ms': round((comp['t'] - origin) / 1e6, 3),
                'done_ms': round((comp['done'] - origin) / 1e6, 3) if comp['done'] else None,
                'res': comp['res']},
            'copies': relation(comp)})
    return {
        'vk_submits': len(subs),
        'vk_submits_by_context': dict(collections.Counter(s['name'] for s in subs)),
        'present_copies': len(copies),
        'scanouts': sorted(scanouts, key=lambda r: int(r) if r.isdigit() else 0),
        'compositions_by_context': dict(by_name),
        'compositions': dict(counts),
        'torn': frames,
    }


def fmt(event, origin):
    t, kind, fields = event
    return f'{(t - origin) / 1e6:+9.3f} ms {kind} ' + ' '.join(f'{k}={v}' for k, v in fields.items())


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('trace')
    ap.add_argument('grab')
    ap.add_argument('--window-ms', type=float, default=50)
    ap.add_argument('--vk', nargs='*', default=[], help='traces of the render workers')
    ap.add_argument('--alias', action='append', default=[], metavar='DST=RES',
                    help='a present copy into DST writes the memory of resource RES')
    args = ap.parse_args()
    events = parse(args.trace)
    vk_events = sorted((e for path in args.vk for e in parse(path)), key=lambda e: e[0])
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
        entry['publication_t'] = origin
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
        **({'submits': submits(events, vk_events, torn, dict(a.split('=', 1) for a in args.alias))} if args.vk else {}),
    }, indent=1))
    return 0


if __name__ == '__main__':
    sys.exit(main())
