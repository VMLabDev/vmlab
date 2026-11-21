ADAPTER = 'Yttrium VirtIO GPU 3D'


def judge(report):
    # P10: the Direct2D coverage probe.
    what = 'Direct2D fills antialiased shapes without gaps'
    d = report.probe('d2d', 'P10', what)
    if d is not None:
        level = int((d.get('adapter') or {}).get('feature_level', '0'), 16)
        note = (f"{d['holes']} holes and {d['spills']} spills against WARP, {d['runs_differ']} pixels differ between "
                f"{d['runs']} runs, {d['edge_diffs']} edge pixels off by more than 64, {d['filled_pixels']} filled pixels")
        if ADAPTER not in (d.get('adapter') or {}).get('name', ''):
            report.row('d2d', 'P10', what, 'not-run', f"the default adapter is {(d.get('adapter') or {}).get('name')!r}", 'd2d.json')
        elif level < 0xb100:
            report.row('d2d', 'P10', what, 'not-run',
                f'the device is feature level 0x{level:04x}; Direct2D takes this path on 11.1 only ({note})', 'd2d.json')
        else:
            ok = not (d['holes'] or d['spills'] or d['runs_differ'])
            report.row('d2d', 'P10', what, 'pass' if ok else 'fail', note, 'd2d.json, d2d.png')
