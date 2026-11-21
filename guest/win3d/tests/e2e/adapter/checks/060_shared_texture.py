def judge(report):
    # P7: the shape of a duplication surface, as an application's own shared texture.
    what = 'A shared BGRA render target works between two devices'
    d = report.probe('shared-texture', 'P7', what)
    if d is not None:
        cases = d.get('cases') or []
        bad = [f"{c.get('size')} {c.get('sharing')}" + (f" ({c['failed']} {c.get('hr')})" if c.get('failed') else '')
               for c in cases if not c.get('match')]
        if d.get('result') != 'ok' or not cases:
            report.row('shared-texture', 'P7', what, 'fail', '; '.join(d.get('errors') or ['the probe shared no texture']),
                'shared-texture.json')
        elif bad:
            report.row('shared-texture', 'P7', what, 'fail',
                f"{len(cases) - len(bad)} of {len(cases)} cases pass; wrong: {', '.join(bad)}", 'shared-texture.json')
        else:
            report.row('shared-texture', 'P7', what, 'pass',
                f"{len(cases)} cases, each read whole in both directions: {d.get('desktop_width')}x{d.get('desktop_height')} "
                f"and 256x256, by legacy handle and with a keyed mutex", 'shared-texture.json')
