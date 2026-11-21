def judge(report):
    # P2: a vblank that belongs to the display.
    names = [('vblank-interval', 'the vblank interval is the refresh period'),
             ('present-rate', 'a vsynced present runs at the refresh rate'),
             ('flip-timestamps', 'frame statistics count the refreshes'),
             ('scan-line', 'the scan line moves and reaches vertical blank')]
    why = report.status('pace')
    d = None if why else report.load('pace.json')
    if why or d is None:
        for c, w in names:
            report.row(c, 'P2', w, 'not-run', why or 'the probe wrote no result')
    elif d.get('errors') and 'present_fps' not in d:
        for c, w in names:
            report.row(c, 'P2', w, 'fail', 'an API call of the probe failed: ' + '; '.join(d['errors'])[:200], 'pace.json')
    else:
        hz = d.get('mode_refresh_hz') or 60
        period = 1000.0 / hz
        v = d.get('vblank_interval_ms') or {}
        if not v.get('n'):
            report.row('vblank-interval', 'P2', names[0][1], 'fail', 'WaitForVBlank never returned', 'pace.json')
        else:
            ok = abs(v['mean'] - period) <= 0.02 * period and v['p01'] >= 0.5 * period and v['p99'] <= 1.5 * period
            report.row('vblank-interval', 'P2', names[0][1], 'pass' if ok else 'fail',
                f"mean {v['mean']:.3f} ms, p01 {v['p01']:.2f}, p99 {v['p99']:.2f}, max {v['max']:.2f} over {v['n']} "
                f"intervals; the {hz} Hz mode is {period:.3f} ms (limits: mean within 2 percent, p01 and p99 within "
                f"half a period)", 'pace.json')
        fps = d.get('present_fps')
        p = d.get('present_interval_ms') or {}
        if not fps:
            report.row('present-rate', 'P2', names[1][1], 'fail', 'no frame was presented', 'pace.json')
        else:
            report.row('present-rate', 'P2', names[1][1], 'pass' if abs(fps - hz) <= 0.05 * hz else 'fail',
                f"{fps:.2f} frames per second with sync interval 1 on a {hz} Hz mode (limit 5 percent); interval p01 "
                f"{p.get('p01', 0):.2f} ms, p99 {p.get('p99', 0):.2f} ms", 'pace.json')
        fs = d.get('frame_statistics') or {}
        if not fs.get('available'):
            report.row('flip-timestamps', 'P2', names[2][1], 'fail', 'GetFrameStatistics gave no sync time', 'pace.json')
        else:
            report.row('flip-timestamps', 'P2', names[2][1], 'pass' if abs(fs['refresh_hz'] - hz) <= 0.02 * hz else 'fail',
                f"{fs['sync_refreshes']} refreshes in {fs['sync_ms']:.0f} ms by SyncRefreshCount and SyncQPCTime: "
                f"{fs['refresh_hz']:.2f} Hz on a {hz} Hz mode (limit 2 percent); {fs['presents']} presents", 'pace.json')
        sl = d.get('scanline') or {}
        if not sl.get('available'):
            report.row('scan-line', 'P2', names[3][1], 'not-run', sl.get('why', 'D3DKMTGetScanLine is not available'), 'pace.json')
        elif sl['failures'] == sl['samples']:
            report.row('scan-line', 'P2', names[3][1], 'fail', f"D3DKMTGetScanLine failed: {sl['first_failure']}", 'pace.json')
        else:
            ok = sl['distinct_lines'] >= 2 and sl['in_vertical_blank'] > 0
            report.row('scan-line', 'P2', names[3][1], 'pass' if ok else 'fail',
                f"{sl['distinct_lines']} different lines (highest {sl['max_line']}) and {sl['in_vertical_blank']} "
                f"answers in vertical blank in {sl['samples']} samples over one second", 'pace.json')
