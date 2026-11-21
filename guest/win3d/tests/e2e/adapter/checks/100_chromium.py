def judge(report):
    # P8: the native d3d12.dll in a Chromium GPU process, its controls, and the bridge for a native program (tests/e2e/chromium.sh).
    what = 'a Chromium GPU process loads the native d3d12.dll and ends itself without it; a native program keeps the bridge'
    why = report.status('chromium')
    d = None if why else report.load('chromium.json')
    if why or d is None:
        report.row('chromium', 'P8', what, 'not-run', why or 'tests/e2e/chromium.sh wrote no verdict')
    else:
        parts = [f"{r.get('check')} {'pass' if r.get('result') == 'pass' else 'FAIL'}" for r in d.get('rows') or []]
        ok = bool(parts) and d.get('passed') == d.get('total')
        report.row('chromium', 'P8', what, 'pass' if ok else 'fail', ', '.join(parts),
            'chromium.json, ../chromium/chrome.json, ../chromium/edge.json, ../chromium/chrome-control.json, '
            '../chromium/edge-control.json, ../chromium/native.json')
