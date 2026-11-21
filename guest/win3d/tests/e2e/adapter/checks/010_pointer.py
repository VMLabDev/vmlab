def judge(report):
    # P1: the pointer is not part of the frame.
    what = 'the pointer is not part of the frame'
    d = report.probe('pointer', 'P1', what)
    if d is not None:
        frames = [f for f in (report.load(f'pointer-frame-{i}.json') for i in (1, 2)) if f]
        samples = [s for s in d.get('samples', []) if 'x' in s]
        showing = [s for s in samples if s.get('showing')]

        def reached(f):
            return any(abs(s['x'] - f['x']) <= 3 and abs(s['y'] - f['y']) <= 3 for s in showing)

        ev = 'pointer.json, pointer-frame-1.json, pointer-frame-2.json, console-pointer-1.png'
        if len(frames) < 2:
            report.row('pointer', 'P1', what, 'not-run', 'no console capture of the probe window', ev)
        elif min(f['background'] for f in frames) < 0.9:
            report.row('pointer', 'P1', what, 'not-run', 'the probe window did not fill the console capture', ev)
        elif not all(reached(f) for f in frames):
            report.row('pointer', 'P1', what, 'not-run',
                f"Windows did not report a visible pointer where the host put it "
                f"({len(showing)} of {len(samples)} samples visible)", ev)
        else:
            near = [f['near'] for f in frames]
            if any(near):
                report.row('pointer', 'P1', what, 'fail',
                    f"Windows drew the pointer into the frame: {near[0]} and {near[1]} pointer pixels at the two places "
                    f"the host moved it to", ev)
            else:
                report.row('pointer', 'P1', what, 'pass',
                    'the capture is the flat fill colour at both places while Windows reports a visible pointer there', ev)
