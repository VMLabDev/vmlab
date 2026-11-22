# The limits of the latency check, in milliseconds from the pointer event to
# the console frame that shows the answer. A person notices a desktop that
# answers later than about 100 ms.
P50_LIMIT_MS = 100
P90_LIMIT_MS = 150


def judge(report):
    # U1: the desktop answers the pointer in time (card #447).
    what = 'the desktop shows the answer to a pointer move in time'
    d = report.probe('latency', 'U1', what)
    if d is None:
        return
    g = report.load('latency.grab.json')
    host = report.load('latency.host.json') or {}
    ev = 'latency.json, latency.grab.json'
    lat = (g or {}).get('latency_ms')
    if g is None or g.get('error') or not lat:
        report.row('latency', 'U1', what, 'not-run',
            'the console measurement failed: ' + str((g or {}).get('error') or 'no pointer move was answered'), ev)
        return
    c = d.get('compose_ms') or {}
    ok = g['timeouts'] == 0 and lat['p50'] <= P50_LIMIT_MS and lat['p90'] <= P90_LIMIT_MS
    report.row('latency', 'U1', what, 'pass' if ok else 'fail',
        f"{lat['n']} of {g['moves']} pointer moves answered, {g['timeouts']} not within {g['timeout_ms']:.0f} ms: "
        f"p50 {lat['p50']:.0f} ms, p90 {lat['p90']:.0f} ms, p99 {lat['p99']:.0f} ms, max {lat['max']:.0f} ms from the "
        f"pointer event to the console frame (limits: p50 {P50_LIMIT_MS} ms, p90 {P90_LIMIT_MS} ms, every move "
        f"answered); the console answers a request every {g.get('poll_ms', 0):.1f} ms; in the guest the wait for "
        f"composition was p50 {c.get('p50', 0):.1f} ms, p90 {c.get('p90', 0):.1f} ms; host load "
        f"{host.get('host_load', 0)} busy processes on {host.get('host_cpus', '?')} CPUs", ev)
