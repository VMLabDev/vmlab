import re


# Wine 11.0 (tests/e2e/PINS). Recheck these source locations when the pin moves.
# C-number references are in docs/acceptance.md and docs/acceptance/C*.md.
CONTROL_FAILURES = {
    'd3d9_d3d9ex': ('d3d9ex.c', {
        687, 705,  # C343: restricted depth-stencil creation
        2949, 3007, 3100,  # C340: WM_DISPLAYCHANGE arrives after the assertion
        # C412: session focus and mode changes, locations from #886 draft D6
        2520, 2915, 2986, 2990, 3009, 3022, 3024, 3203, 3209,
    }),
    'd3d9_device': ('device.c', {
        4248, 4319, 4410,  # C413/C414: WM_DISPLAYCHANGE timing (C340)
        5179, 5184, 5215, 5220,  # C413: window styles reproduced without Win3D
        15008, 15291,  # C412/C413: window rectangle and control refcount
        4656, 4658, 4675, 4677, 4871, 15043, 15049,  # C412/C413/C414: session focus
        3790, 4545, 4551, 4568,  # C414 and #886 draft D6: window messages
    }),
}
ASSERTION_FAILURE = re.compile(
    r"^([^:\n]+):(\d+): Test (?:failed|succeeded inside todo block): ",
    re.MULTILINE,
)


def control_verdict(report, name, count):
    """Compare each reported failure to the control locations."""
    if not count or name not in CONTROL_FAILURES:
        return not count, ''

    filename, locations = CONTROL_FAILURES[name]
    try:
        log = (report.directory / f'wine-{name}.failures.txt').read_text(encoding='utf-8-sig')
    except (OSError, UnicodeError):
        return False, ', failure log unavailable'
    failures = ASSERTION_FAILURE.findall(log)
    if len(failures) != count:
        return False, f', failure log has {len(failures)} assertion lines for {count} reported failures'
    unexpected = dict.fromkeys(
        f'{source}:{line}' for source, line in failures
        if source != filename or int(line) not in locations
    )
    if unexpected:
        return False, ', outside the control list: ' + ', '.join(list(unexpected)[:5])
    return True, f', all {count} failures match the control list'


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
            passed, control_note = control_verdict(report, name, d['failures'])
            report.row(check, 'P10', what, 'pass' if passed else 'fail', note + control_note, ev)
        else:
            report.row(check, 'P10', what, 'fail',
                f"ended with exit code {code} after {d['duration_s']:.0f} s, before its summary; {d['failed_lines']} "
                f"failed checks printed", ev)
