def judge(report):
    # P1: the shapes, positions and visibility of the pointer reach the host (tests/e2e/pointer.sh).
    what = "the pointer's shapes, positions and hiding reach the host"
    why = report.status('pointer-shapes')
    d = None if why else report.load('pointer-shapes.json')
    if why or d is None:
        report.row('pointer-shapes', 'P1', what, 'not-run', why or 'tests/e2e/pointer.sh wrote no verdict')
    else:
        parts = [f"{k} {'pass' if v.get('pass') else 'FAIL'}" for k, v in d.items() if isinstance(v, dict) and 'pass' in v]
        errors = d.get('watch_errors') or []
        report.row('pointer-shapes', 'P1', what, 'pass' if d.get('pass') else 'fail',
            ', '.join(parts) + (f"; watch errors: {'; '.join(map(str, errors))[:160]}" if errors else ''),
            'pointer-shapes.json, ../pointer/events.json, ../pointer/pointer.mp4')
