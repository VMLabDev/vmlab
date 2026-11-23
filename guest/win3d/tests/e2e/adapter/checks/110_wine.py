def judge(report):
    # P10: Wine's Direct3D tests, one row per test program.
    why = report.status('wine')
    try:
        with open(report.directory / 'wine.programs') as f:
            programs = f.read().split()
    except OSError:
        programs = []
    if why or not programs:
        report.row('wine', 'P10', "Wine's Direct3D 9, 10 and 11 tests", 'not-run', why or 'no test program is listed')
    for name in programs:
        check = f'wine-{name}'
        module = next(m for m in ('d3d10_1', 'd3d10core', 'd3d10', 'd3d11', 'd3d9', '') if name.startswith(m + '_') or not m)
        test = name[len(module) + 1:] if module else name
        what = f"Wine's {module} test {test} passes"
        d = report.load(f'{check}.wine.json')
        ev = f'{check}.wine.json, {check}.failures.txt'
        if d is None:
            report.row(check, 'P10', what, 'not-run', 'the program was not run, or the guest stopped answering')
            continue
        code = d.get('exit_code')
        if d.get('vm_lost'):
            report.row(check, 'P10', what, 'fail',
                f"the VM stopped {d['duration_s']} s into the program: {d.get('hypervisor') or 'vmlab-hv left no exit line'}", ev)
        elif d.get('timed_out'):
            report.row(check, 'P10', what, 'fail',
                f"stopped at its time limit after {d['duration_s']:.0f} s; {d['failed_lines']} failed checks printed", ev)
        elif d.get('crashed') or (code is not None and not 0 <= code <= 255):
            report.row(check, 'P10', what, 'fail',
                f"ended on an unhandled exception (exit code 0x{(code or 0) & 0xffffffff:08x}) after {d['duration_s']:.0f} s; "
                f"{d['failed_lines']} failed checks printed before it", ev)
        elif d.get('completed'):
            note = (f"{d['executed']} tests executed, {d['failures']} failures, {d['todo']} todo, {d['skipped']} skipped "
                    f"in {d['duration_s']:.0f} s")
            report.row(check, 'P10', what, 'pass' if not d['failures'] else 'fail', note, ev)
        else:
            report.row(check, 'P10', what, 'fail',
                f"ended with exit code {code} after {d['duration_s']:.0f} s, before its summary; {d['failed_lines']} "
                f"failed checks printed", ev)
