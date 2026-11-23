#!/usr/bin/env python3
"""Write the results tables in docs/acceptance.md from each template's results.json.

    tests/e2e/report.py [--adapter-only]

Replaces the text between the `<!-- e2e:results -->` and
`<!-- /e2e:results -->` markers with one table per evidence directory that
has a results.json: docs/evidence/acceptance/<template>/ (AMD1), then
<template>-<gpu>/ (run.sh --host-gpu), plus the host GPU, QEMU and commit
that run recorded in host.txt.

Does the same between `<!-- e2e:adapter -->` and `<!-- /e2e:adapter -->` for
the adapter suite, from each directory's adapter/adapter.json
(tests/e2e/adapter.sh). --adapter-only leaves the results tables as they are.
"""
import json, os, re, sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DOC = os.path.join(REPO, 'docs', 'acceptance.md')
EVROOT = os.path.join(REPO, 'docs', 'evidence', 'acceptance')


def cell(s):
    return str(s).replace('|', '\\|').replace('\n', ' ')


def hosts(ev):
    host = {}
    for line in open(os.path.join(ev, 'host.txt')):
        k, _, v = line.partition(': ')
        host[k.strip()] = v.strip()
    return host


def replace(doc, marker, body):
    new = f'<!-- {marker} -->\n' + '\n'.join(body) + f'<!-- /{marker} -->'
    doc2 = re.sub(rf'<!-- {marker} -->.*?<!-- /{marker} -->', lambda _: new, doc, flags=re.S)
    if doc2 == doc and new not in doc:
        sys.exit(f'markers {marker} not found in docs/acceptance.md')
    return doc2


out = []
adapter = []
dirs = ['windows-11', 'windows-10']
dirs += sorted(d for d in os.listdir(EVROOT) if d not in dirs and re.match(r'windows-1[01]-', d))
for t in dirs:
    ev = os.path.join(EVROOT, t)
    try:
        ares = json.load(open(os.path.join(ev, 'adapter', 'adapter.json')))
    except OSError:
        ares = None
    if ares:
        host = hosts(ev)
        c = ares['counts']
        adapter.append(f'### {t}\n')
        adapter.append(f"Host GPU: {host.get('venus gpu', '?').removeprefix('host_gpu ')}. Run {host.get('date', '?')} "
                       f"at commit `{host.get('commit', '?')[:12]}`, template `{host.get('template', '?')}`. "
                       f"**{c['pass']} pass, {c['fail']} fail, {c['not-run']} not-run.**\n")
        adapter.append('| Property | Check | Result | Measured |')
        adapter.append('| --- | --- | --- | --- |')
        for r in ares['rows']:
            adapter.append(f"| {r['property']} | `{r['check']}`: {cell(r['what'])} | **{r['result']}** | {cell(r['note'])} |")
        adapter.append('')
    try:
        res = json.load(open(os.path.join(ev, 'results.json')))
    except OSError:
        continue
    host = hosts(ev)
    stage = ''
    m = re.search(r'^stage: (.*)$', open(os.path.join(ev, 'results.txt')).read(), re.M)
    if m:
        stage = m.group(1)
    rows = res['rows']
    counted = [r for r in rows if r['counts']]
    passed = sum(1 for r in counted if r['result'] == 'pass')
    out.append(f'### {t}\n')
    gpu = host.get('venus gpu', '')
    if gpu.startswith('host_gpu '):
        out.append(f"Host GPU: {gpu.removeprefix('host_gpu ')}. Hypervisor: "
                   f"{host.get('hypervisor', host.get('qemu (PATH)', '?'))}.\n")
    out.append(f"Run {host.get('date', '?')} at commit `{host.get('commit', '?')[:12]}`, template "
               f"`{host.get('template', '?')}`. **{'PASS' if res['passed'] else 'FAIL'}: {passed} of "
               f"{len(counted)} Done checks pass** (run stage: {stage or '?'}). Evidence: "
               f"`docs/evidence/acceptance/{t}/`; `results.txt` is the script's own table.\n")
    out.append('| Done item | Check | Result | Evidence | Note |')
    out.append('| --- | --- | --- | --- | --- |')
    for r in rows:
        item = r['item'] if r['counts'] else f"{r['item']} (extra)"
        res_s = '**pass**' if r['result'] == 'pass' else '**FAIL**'
        evid = ', '.join(f'`{e.strip()}`' for e in r['evidence'].split(','))
        out.append(f"| {item} | {cell(r['check'])} | {res_s} | {evid} | {cell(r['note'])} |")
    out.append('')

doc = open(DOC).read()
if '--adapter-only' not in sys.argv[1:]:
    doc = replace(doc, 'e2e:results', out)
doc = replace(doc, 'e2e:adapter', adapter)
open(DOC, 'w').write(doc)
print(f'updated {DOC}')
