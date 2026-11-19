#!/usr/bin/env python3
"""Judge one run of the adapter suite from its evidence directory.

    judge.py <evidence-dir>/adapter

Reads what tests/e2e/adapter.sh collected, writes adapter.json and adapter.txt
into the directory and prints the table. Every check ends as one of

  pass      the property holds
  fail      the property was measured and does not hold
  not-run   nothing was measured: the check was not selected, its probe did
            not start, or what it needs is missing. The note says which.

The thresholds are here and nowhere else; the probes only measure. Exit 1
when a check fails. A check that did not run is not a failure.
"""
import json
import os
import re
import sys

EV = sys.argv[1]
rows = []
ADAPTER = 'Yttrium VirtIO GPU 3D'
# A WebGL renderer string that is not this adapter: the page then says nothing about it.
SOFTWARE = re.compile(r'SwiftShader|Basic Render|WARP|llvmpipe', re.I)


def load(name):
    try:
        with open(os.path.join(EV, name), encoding='utf-8-sig') as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def status(check):
    """None when the check ran, else why it did not."""
    try:
        with open(os.path.join(EV, check + '.status')) as f:
            s = f.read().strip()
    except OSError:
        return 'the suite did not reach this check'
    return None if s == 'ran' else s.removeprefix('not-run: ')


def row(check, prop, what, result, note, evidence=''):
    rows.append({'check': check, 'property': prop, 'what': what, 'result': result, 'note': note,
                 'evidence': evidence})


def probe(check, prop, what, name=None):
    """The probe's JSON, or None after recording why there is none."""
    why = status(check)
    if why:
        row(check, prop, what, 'not-run', why)
        return None
    d = load((name or check) + '.json')
    if d is None:
        row(check, prop, what, 'not-run', 'the probe wrote no result')
        return None
    if d.get('errors'):
        # The probe ran on the adapter and an API call failed: the property does not hold.
        row(check, prop, what, 'fail', 'an API call of the probe failed: ' + '; '.join(d['errors'])[:200],
            (name or check) + '.json')
        return None
    return d


# P1: the pointer is not part of the frame.
what = 'the pointer is not part of the frame'
d = probe('pointer', 'P1', what)
if d is not None:
    frames = [f for f in (load(f'pointer-frame-{i}.json') for i in (1, 2)) if f]
    samples = [s for s in d.get('samples', []) if 'x' in s]
    showing = [s for s in samples if s.get('showing')]

    def reached(f):
        return any(abs(s['x'] - f['x']) <= 3 and abs(s['y'] - f['y']) <= 3 for s in showing)

    ev = 'pointer.json, pointer-frame-1.json, pointer-frame-2.json, console-pointer-1.png'
    if len(frames) < 2:
        row('pointer', 'P1', what, 'not-run', 'no console capture of the probe window', ev)
    elif min(f['background'] for f in frames) < 0.9:
        row('pointer', 'P1', what, 'not-run', 'the probe window did not fill the console capture', ev)
    elif not all(reached(f) for f in frames):
        row('pointer', 'P1', what, 'not-run',
            f"Windows did not report a visible pointer where the host put it "
            f"({len(showing)} of {len(samples)} samples visible)", ev)
    else:
        near = [f['near'] for f in frames]
        if any(near):
            row('pointer', 'P1', what, 'fail',
                f"Windows drew the pointer into the frame: {near[0]} and {near[1]} pointer pixels at the two places "
                f"the host moved it to", ev)
        else:
            row('pointer', 'P1', what, 'pass',
                'the capture is the flat fill colour at both places while Windows reports a visible pointer there', ev)

# P2: a vblank that belongs to the display.
names = [('vblank-interval', 'the vblank interval is the refresh period'),
         ('present-rate', 'a vsynced present runs at the refresh rate'),
         ('flip-timestamps', 'frame statistics count the refreshes'),
         ('scan-line', 'the scan line moves and reaches vertical blank')]
why = status('pace')
d = None if why else load('pace.json')
if why or d is None:
    for c, w in names:
        row(c, 'P2', w, 'not-run', why or 'the probe wrote no result')
elif d.get('errors') and 'present_fps' not in d:
    for c, w in names:
        row(c, 'P2', w, 'fail', 'an API call of the probe failed: ' + '; '.join(d['errors'])[:200], 'pace.json')
else:
    hz = d.get('mode_refresh_hz') or 60
    period = 1000.0 / hz
    v = d.get('vblank_interval_ms') or {}
    if not v.get('n'):
        row('vblank-interval', 'P2', names[0][1], 'fail', 'WaitForVBlank never returned', 'pace.json')
    else:
        ok = abs(v['mean'] - period) <= 0.02 * period and v['p01'] >= 0.5 * period and v['p99'] <= 1.5 * period
        row('vblank-interval', 'P2', names[0][1], 'pass' if ok else 'fail',
            f"mean {v['mean']:.3f} ms, p01 {v['p01']:.2f}, p99 {v['p99']:.2f}, max {v['max']:.2f} over {v['n']} "
            f"intervals; the {hz} Hz mode is {period:.3f} ms (limits: mean within 2 percent, p01 and p99 within "
            f"half a period)", 'pace.json')
    fps = d.get('present_fps')
    p = d.get('present_interval_ms') or {}
    if not fps:
        row('present-rate', 'P2', names[1][1], 'fail', 'no frame was presented', 'pace.json')
    else:
        row('present-rate', 'P2', names[1][1], 'pass' if abs(fps - hz) <= 0.05 * hz else 'fail',
            f"{fps:.2f} frames per second with sync interval 1 on a {hz} Hz mode (limit 5 percent); interval p01 "
            f"{p.get('p01', 0):.2f} ms, p99 {p.get('p99', 0):.2f} ms", 'pace.json')
    fs = d.get('frame_statistics') or {}
    if not fs.get('available'):
        row('flip-timestamps', 'P2', names[2][1], 'fail', 'GetFrameStatistics gave no sync time', 'pace.json')
    else:
        row('flip-timestamps', 'P2', names[2][1], 'pass' if abs(fs['refresh_hz'] - hz) <= 0.02 * hz else 'fail',
            f"{fs['sync_refreshes']} refreshes in {fs['sync_ms']:.0f} ms by SyncRefreshCount and SyncQPCTime: "
            f"{fs['refresh_hz']:.2f} Hz on a {hz} Hz mode (limit 2 percent); {fs['presents']} presents", 'pace.json')
    sl = d.get('scanline') or {}
    if not sl.get('available'):
        row('scan-line', 'P2', names[3][1], 'not-run', sl.get('why', 'D3DKMTGetScanLine is not available'), 'pace.json')
    elif sl['failures'] == sl['samples']:
        row('scan-line', 'P2', names[3][1], 'fail', f"D3DKMTGetScanLine failed: {sl['first_failure']}", 'pace.json')
    else:
        ok = sl['distinct_lines'] >= 2 and sl['in_vertical_blank'] > 0
        row('scan-line', 'P2', names[3][1], 'pass' if ok else 'fail',
            f"{sl['distinct_lines']} different lines (highest {sl['max_line']}) and {sl['in_vertical_blank']} "
            f"answers in vertical blank in {sl['samples']} samples over one second", 'pace.json')

# P3 to P5: a flip shows a finished frame.
for mode, kind in (('flip', 'flip-model presents'), ('blt', 'presents of the Blt kind')):
    check = f'tear-{mode}'
    what = f'no torn frame on the console, {kind}'
    d = probe(check, 'P3-P5', what)
    if d is None:
        continue
    g = load(f'{check}.grab.json')
    ev = f'{check}.json, {check}.grab.json'
    if g is None or g.get('error'):
        row(check, 'P3-P5', what, 'not-run', 'the console grab failed: ' + str((g or {}).get('error')), ev)
    elif g['window_frames'] < 100 or len(g['colours_seen']) < 3:
        row(check, 'P3-P5', what, 'not-run',
            f"the grab saw the probe window in {g['window_frames']} of {g['frames']} frames and "
            f"{len(g['colours_seen'])} of its colours", ev)
    else:
        note = (f"{g['torn_frames']} torn of {g['window_frames']} console frames ({g['torn_persisting']} of them "
                f"unchanged from the frame before), {g['fps']} grabs per second; the probe presented "
                f"{d.get('frames_presented')} frames at {d.get('animating_fps', 0):.1f} per second")
        if g['torn_frames']:
            ev += f', {check}-torn.png'
        row(check, 'P3-P5', what, 'fail' if g['torn_frames'] else 'pass', note, ev)

# P7: Desktop Duplication.
what = 'Desktop Duplication delivers lit frames'
d = probe('duplication', 'P7', what)
if d is not None:
    frames = d.get('frames') or []
    good = [f for f in frames if f.get('match') and f.get('lit_pixels', 0) >= 0.5 * f.get('pixels', 1)]
    hrs = sorted({f['hr'] for f in frames if f.get('hr') != '0x00000000'})
    if d.get('duplicate_output_hr') != '0x00000000':
        row('duplication', 'P7', what, 'fail', f"DuplicateOutput failed: {d.get('duplicate_output_hr')}",
            'duplication.json')
    elif frames and len(good) == len(frames):
        lit = min(f['lit_pixels'] / f['pixels'] for f in frames)
        row('duplication', 'P7', what, 'pass',
            f"{len(frames)} frames, each with the window in the colour it had; at least {lit:.1%} of a frame is lit",
            'duplication.json')
    else:
        row('duplication', 'P7', what, 'fail',
            f"{len(good)} of {len(frames)} frames are lit and show the window's colour"
            + (f"; AcquireNextFrame answered {', '.join(hrs)}" if hrs else ''), 'duplication.json')

# P10: the Direct2D coverage probe.
what = 'Direct2D fills antialiased shapes without gaps'
d = probe('d2d', 'P10', what)
if d is not None:
    level = int((d.get('adapter') or {}).get('feature_level', '0'), 16)
    note = (f"{d['holes']} holes and {d['spills']} spills against WARP, {d['runs_differ']} pixels differ between "
            f"{d['runs']} runs, {d['edge_diffs']} edge pixels off by more than 64, {d['filled_pixels']} filled pixels")
    if ADAPTER not in (d.get('adapter') or {}).get('name', ''):
        row('d2d', 'P10', what, 'not-run', f"the default adapter is {(d.get('adapter') or {}).get('name')!r}", 'd2d.json')
    elif level < 0xb100:
        row('d2d', 'P10', what, 'not-run',
            f'the device is feature level 0x{level:04x}; Direct2D takes this path on 11.1 only ({note})', 'd2d.json')
    else:
        ok = not (d['holes'] or d['spills'] or d['runs_differ'])
        row('d2d', 'P10', what, 'pass' if ok else 'fail', note, 'd2d.json, d2d.png')

# P10 and P8: the two pages, in Chrome and in Edge.
for browser in ('chrome', 'edge'):
    label = 'Chrome' if browser == 'chrome' else 'Edge'
    for page, prop in (('svb', 'P10'), ('venc', 'P8')):
        check = f'{page}-{browser}'
        what = (f'WebGL draws from a STATIC_DRAW vertex buffer in {label}' if page == 'svb' else
                f"{label}'s GPU process survives the video encoder query")
        why = status(check)
        d = None if why else load(f'{check}.page.json')
        ev = f'{check}.page.json, console-{check}.png'
        if why or d is None:
            row(check, prop, what, 'not-run', why or 'Run-Page.ps1 wrote no result')
            continue
        r = d.get('result')
        who = f"{label} {d.get('version')}"
        if not d.get('reported') or not r:
            row(check, prop, what, 'not-run', f"{who}: the page did not report (browser log: {d.get('log_lines')} lines)", ev)
        elif page == 'svb':
            lines = r.get('lines') or {}
            text = ', '.join(f'{k} {v}' for k, v in lines.items())
            renderer = r.get('renderer') or ''
            if not renderer or SOFTWARE.search(renderer) or 'NO WEBGL' in lines.values():
                row(check, prop, what, 'not-run', f"{who}: WebGL is not on the adapter (renderer {renderer!r}; {text})", ev)
            else:
                ok = len(lines) == 3 and all(v == '255,0,0,255' for v in lines.values())
                row(check, prop, what, 'pass' if ok else 'fail', f"{who}: {text}; {renderer}", ev)
        else:
            before, after = r.get('before') or '', r.get('after') or []
            if (before == 'NO WEBGL' or SOFTWARE.search(before)) and d.get('crash_dumps'):
                row(check, prop, what, 'fail',
                    f"{who}: no WebGL on the adapter when the page loaded ({before!r}) and {d['crash_dumps']} crash "
                    f"dumps: the GPU process ended before the page's first WebGL context", ev)
            elif before == 'NO WEBGL' or SOFTWARE.search(before):
                row(check, prop, what, 'not-run', f"{who}: WebGL was not on the adapter before the query ({before!r})", ev)
            else:
                lost = [a for a in after if a == 'NO WEBGL' or SOFTWARE.search(a)]
                ok = len(after) == 5 and not lost and not d.get('crash_dumps')
                row(check, prop, what, 'pass' if ok else 'fail',
                    f"{who}: WebGL on the adapter in {len(after) - len(lost)} of {len(after)} tries over 20 s after "
                    f"the query, {d.get('crash_dumps')} crash dumps; encoder {r.get('encoder')}; WebGPU {r.get('webgpu')}", ev)

# P10: Wine's Direct3D tests, one row per test program.
why = status('wine')
try:
    with open(os.path.join(EV, 'wine.programs')) as f:
        programs = f.read().split()
except OSError:
    programs = []
if why or not programs:
    row('wine', 'P10', "Wine's Direct3D 9, 10 and 11 tests", 'not-run', why or 'no test program is listed')
for name in programs:
    check = f'wine-{name}'
    module = next(m for m in ('d3d10_1', 'd3d10core', 'd3d10', 'd3d11', 'd3d9', '') if name.startswith(m + '_') or not m)
    test = name[len(module) + 1:] if module else name
    what = f"Wine's {module} test {test} passes"
    d = load(f'{check}.wine.json')
    ev = f'{check}.wine.json, {check}.failures.txt'
    if d is None:
        row(check, 'P10', what, 'not-run', 'the program was not run, or the guest stopped answering')
        continue
    code = d.get('exit_code')
    if d.get('vm_lost'):
        row(check, 'P10', what, 'fail',
            f"the VM stopped {d['duration_s']} s into the program: {d.get('hypervisor') or 'vmlab-hv left no exit line'}", ev)
    elif d.get('timed_out'):
        row(check, 'P10', what, 'fail',
            f"stopped at its time limit after {d['duration_s']:.0f} s; {d['failed_lines']} failed checks printed", ev)
    elif d.get('crashed') or (code is not None and not 0 <= code <= 255):
        row(check, 'P10', what, 'fail',
            f"ended on an unhandled exception (exit code 0x{(code or 0) & 0xffffffff:08x}) after {d['duration_s']:.0f} s; "
            f"{d['failed_lines']} failed checks printed before it", ev)
    elif d.get('completed'):
        note = (f"{d['executed']} tests executed, {d['failures']} failures, {d['todo']} todo, {d['skipped']} skipped "
                f"in {d['duration_s']:.0f} s")
        row(check, 'P10', what, 'pass' if not d['failures'] else 'fail', note, ev)
    else:
        row(check, 'P10', what, 'fail',
            f"ended with exit code {code} after {d['duration_s']:.0f} s, before its summary; {d['failed_lines']} "
            f"failed checks printed", ev)

count = {r: sum(1 for x in rows if x['result'] == r) for r in ('pass', 'fail', 'not-run')}
with open(os.path.join(EV, 'adapter.json'), 'w') as f:
    json.dump({'evidence_dir': EV, 'counts': count, 'rows': rows}, f, indent=2)
lines = [f"{'Prop':<6} {'Result':<8} Check", '-' * 78]
for r in rows:
    lines.append(f"{r['property']:<6} {r['result']:<8} {r['check']}: {r['what']}")
    lines.append(f"{'':<15} {r['note']}")
lines += ['-' * 78, f"adapter suite: {count['pass']} pass, {count['fail']} fail, {count['not-run']} not-run"]
text = '\n'.join(lines) + '\n'
with open(os.path.join(EV, 'adapter.txt'), 'w') as f:
    f.write(text)
sys.stdout.write(text)
sys.exit(1 if count['fail'] else 0)
