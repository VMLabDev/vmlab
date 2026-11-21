def judge(report):
    # P3 to P5: a flip shows a finished frame.
    for mode, kind in (('flip', 'flip-model presents'), ('blt', 'presents of the Blt kind')):
        check = f'tear-{mode}'
        what = f'no torn frame on the console, {kind}'
        d = report.probe(check, 'P3-P5', what)
        if d is None:
            continue
        g = report.load(f'{check}.grab.json')
        ev = f'{check}.json, {check}.grab.json'
        if g is None or g.get('error'):
            report.row(check, 'P3-P5', what, 'not-run', 'the console grab failed: ' + str((g or {}).get('error')), ev)
        elif g['window_frames'] < 100 or len(g['colours_seen']) < 3:
            report.row(check, 'P3-P5', what, 'not-run',
                f"the grab saw the probe window in {g['window_frames']} of {g['frames']} frames and "
                f"{len(g['colours_seen'])} of its colours", ev)
        else:
            note = (f"{g['torn_frames']} torn of {g['window_frames']} console frames ({g['torn_persisting']} of them "
                    f"unchanged from the frame before), {g['fps']} grabs per second; the probe presented "
                    f"{d.get('frames_presented')} frames at {d.get('animating_fps', 0):.1f} per second")
            if g['torn_frames']:
                ev += f', {check}-torn.png'
            report.row(check, 'P3-P5', what, 'fail' if g['torn_frames'] else 'pass', note, ev)
