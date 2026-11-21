def judge(report):
    # P7: Desktop Duplication.
    what = 'Desktop Duplication delivers lit frames'
    d = report.probe('duplication', 'P7', what)
    if d is not None:
        frames = d.get('frames') or []
        good = [f for f in frames if f.get('match') and f.get('lit_pixels', 0) >= 0.5 * f.get('pixels', 1)]
        hrs = sorted({f['hr'] for f in frames if f.get('hr') != '0x00000000'})
        if d.get('duplicate_output_hr') != '0x00000000':
            report.row('duplication', 'P7', what, 'fail', f"DuplicateOutput failed: {d.get('duplicate_output_hr')}",
                'duplication.json')
        elif frames and len(good) == len(frames):
            lit = min(f['lit_pixels'] / f['pixels'] for f in frames)
            report.row('duplication', 'P7', what, 'pass',
                f"{len(frames)} frames, each with the window in the colour it had; at least {lit:.1%} of a frame is lit",
                'duplication.json')
        else:
            report.row('duplication', 'P7', what, 'fail',
                f"{len(good)} of {len(frames)} frames are lit and show the window's colour"
                + (f"; AcquireNextFrame answered {', '.join(hrs)}" if hrs else ''), 'duplication.json')
