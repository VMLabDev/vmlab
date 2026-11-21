def judge(report):
    # P10: target-independent rasterization, the x64 and the x86 probe (tests/e2e/tir.sh).
    what = 'Direct2D antialiasing draws the coverage that WARP draws'
    why = report.status('d2d-tir')
    try:
        with open(report.directory / 'd2d-tir.txt') as f:
            verdicts = [line.split(None, 1) for line in f if line.split(None, 1)[:1] in (['pass'], ['fail'])]
    except OSError:
        verdicts = []
    if why or not verdicts:
        report.row('d2d-tir', 'P10', what, 'not-run', why or 'tests/e2e/tir.sh wrote no verdict')
    else:
        ok = len(verdicts) == 2 and all(v[0] == 'pass' for v in verdicts)
        report.row('d2d-tir', 'P10', what, 'pass' if ok else 'fail',
            '; '.join(f'{v[0]} {v[1].strip()}' for v in verdicts) + ('' if len(verdicts) == 2 else '; one probe gave no verdict'),
            'd2d-tir.txt, ../tir/tir-x64.json, ../tir/tir-x86.json, ../tir/driver-log.txt')
