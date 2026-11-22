def judge(report):
    # S1: sound without breaks under host load (card #447).
    what = 'a tone plays to the host without a break'
    why = report.status('audio')
    if why:
        report.row('audio', 'S1', what, 'not-run', why)
        return
    d = report.probe('audio', 'S1', what, 'tone')
    if d is None:
        return
    g = report.load('tone.capture.json')
    host = report.load('tone.host.json') or {}
    ev = 'tone.json, tone.capture.json'
    if g is None or g.get('error'):
        report.row('audio', 'S1', what, 'not-run', 'the host capture failed: ' + str((g or {}).get('error')), ev)
        return
    seconds = host.get('seconds') or 60
    hv = g.get('hv') or {}
    whole = g['signal_seconds'] >= 0.9 * seconds
    ok = whole and g['discontinuities'] == 0 and g['gaps'] == 0
    report.row('audio', 'S1', what, 'pass' if ok else 'fail',
        f"{g['discontinuities']} discontinuities and {g['gaps']} silent gaps ({g['gap_seconds'] * 1000:.0f} ms) in "
        f"{g['signal_seconds']:.1f} s of a {g['hz']:.0f} Hz sine captured from the VM's host node, of {seconds} s "
        f"played (limits: none, and 90 percent of the tone captured); vmlab-hv logged {hv.get('underruns', '?')} "
        f"underruns with {hv.get('silence_frames', '?')} frames of silence and {hv.get('late_callbacks', '?')} late "
        f"callbacks; the guest's player woke late {d.get('late_wakes')} times and found the buffer empty "
        f"{d.get('empty_wakes')} times ({d.get('rate')} Hz, {d.get('sample_type')}); host load "
        f"{host.get('host_load', 0)} busy processes on {host.get('host_cpus', '?')} CPUs", ev)
