import re


SOFTWARE = re.compile(r'SwiftShader|Basic Render|WARP|llvmpipe', re.I)


def judge(report):
    # P10 and P8: the two pages, in Chrome and in Edge.
    for browser in ('chrome', 'edge'):
        label = 'Chrome' if browser == 'chrome' else 'Edge'
        for page, prop in (('svb', 'P10'), ('venc', 'P8')):
            check = f'{page}-{browser}'
            what = (f'WebGL draws from a STATIC_DRAW vertex buffer in {label}' if page == 'svb' else
                    f"{label}'s GPU process survives the video encoder query")
            why = report.status(check)
            d = None if why else report.load(f'{check}.page.json')
            ev = f'{check}.page.json, console-{check}.png'
            if why or d is None:
                report.row(check, prop, what, 'not-run', why or 'Run-Page.ps1 wrote no result')
                continue
            r = d.get('result')
            who = f"{label} {d.get('version')}"
            if not d.get('reported') or not r:
                report.row(check, prop, what, 'not-run', f"{who}: the page did not report (browser log: {d.get('log_lines')} lines)", ev)
            elif page == 'svb':
                lines = r.get('lines') or {}
                text = ', '.join(f'{k} {v}' for k, v in lines.items())
                renderer = r.get('renderer') or ''
                if not renderer or SOFTWARE.search(renderer) or 'NO WEBGL' in lines.values():
                    report.row(check, prop, what, 'not-run', f"{who}: WebGL is not on the adapter (renderer {renderer!r}; {text})", ev)
                else:
                    ok = len(lines) == 3 and all(v == '255,0,0,255' for v in lines.values())
                    report.row(check, prop, what, 'pass' if ok else 'fail', f"{who}: {text}; {renderer}", ev)
            else:
                before, after = r.get('before') or '', r.get('after') or []
                if (before == 'NO WEBGL' or SOFTWARE.search(before)) and d.get('crash_dumps'):
                    report.row(check, prop, what, 'fail',
                        f"{who}: no WebGL on the adapter when the page loaded ({before!r}) and {d['crash_dumps']} crash "
                        f"dumps: the GPU process ended before the page's first WebGL context", ev)
                elif before == 'NO WEBGL' or SOFTWARE.search(before):
                    report.row(check, prop, what, 'not-run', f"{who}: WebGL was not on the adapter before the query ({before!r})", ev)
                else:
                    lost = [a for a in after if a == 'NO WEBGL' or SOFTWARE.search(a)]
                    ok = len(after) == 5 and not lost and not d.get('crash_dumps')
                    report.row(check, prop, what, 'pass' if ok else 'fail',
                        f"{who}: WebGL on the adapter in {len(after) - len(lost)} of {len(after)} tries over 20 s after "
                        f"the query, {d.get('crash_dumps')} crash dumps; encoder {r.get('encoder')}; WebGPU {r.get('webgpu')}", ev)
