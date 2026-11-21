def judge(report):
    # P10: three draws the driver once dropped (card #328), each followed by a use of the device.
    what = 'a B8G8R8X8 vertex colour and an indexed draw without an index buffer draw, and the device stays usable'
    d = report.probe('draws', 'P10', what)
    if d is not None:
        cases = d.get('cases') or []
        bad = []
        for c in cases:
            if c.get('pass'):
                continue
            if c.get('failed'):
                why = f"{c['failed']} {c.get('hr')}"
            elif not c.get('device_usable'):
                why = f"the device is not usable afterwards, removed reason {c.get('removed_reason')}"
            elif 'first_vertex_only' in c:
                why = f"points drawn {c.get('points_drawn')}, {c.get('other_pixels')} other pixels"
            else:
                why = f"read {c.get('pixel')}, expected {c.get('expected_rgb')}"
            bad.append(f"{c.get('case')} ({why})")
        if d.get('result') != 'ok' or not cases:
            report.row('draws', 'P10', what, 'fail', '; '.join(d.get('errors') or ['the probe made no draw']), 'draws.json')
        elif bad:
            report.row('draws', 'P10', what, 'fail', f"{len(cases) - len(bad)} of {len(cases)} cases pass; wrong: {'; '.join(bad)}",
                'draws.json')
        else:
            report.row('draws', 'P10', what, 'pass',
                f"{len(cases)} cases, each on a device of its own: the draw is on the target, an ordinary draw after it "
                f"too, and GetDeviceRemovedReason answers S_OK", 'draws.json')
