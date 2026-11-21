#!/usr/bin/env python3
"""Judge one run of the adapter suite from its evidence directory.

    judge.py <evidence-dir>/adapter

Reads what tests/e2e/adapter.sh collected, writes adapter.json and adapter.txt
into the directory and prints the table. Every check ends as one of

  pass      the property holds
  fail      the property was measured and does not hold
  not-run   nothing was measured: the check was not selected, its probe did
            not start, or what it needs is missing. The note says which.

The thresholds are in adapter/checks/. Exit 1 when a check fails.
A check that did not run is not a failure.
"""
from importlib import import_module
import json
from pathlib import Path
import sys


class Report:
    def __init__(self, directory):
        self.directory = Path(directory)
        self.rows = []

    def load(self, name):
        try:
            with open(self.directory / name, encoding='utf-8-sig') as f:
                return json.load(f)
        except (OSError, ValueError):
            return None

    def status(self, check):
        """None when the check ran, else why it did not."""
        try:
            with open(self.directory / (check + '.status')) as f:
                s = f.read().strip()
        except OSError:
            return 'the suite did not reach this check'
        return None if s == 'ran' else s.removeprefix('not-run: ')

    def row(self, check, prop, what, result, note, evidence=''):
        self.rows.append({'check': check, 'property': prop, 'what': what, 'result': result, 'note': note,
                          'evidence': evidence})

    def probe(self, check, prop, what, name=None):
        """The probe's JSON, or None after recording why there is none."""
        why = self.status(check)
        if why:
            self.row(check, prop, what, 'not-run', why)
            return None
        d = self.load((name or check) + '.json')
        if d is None:
            self.row(check, prop, what, 'not-run', 'the probe wrote no result')
            return None
        if d.get('errors'):
            # The probe ran on the adapter and an API call failed: the property does not hold.
            self.row(check, prop, what, 'fail', 'an API call of the probe failed: ' + '; '.join(d['errors'])[:200],
                (name or check) + '.json')
            return None
        return d


EV = sys.argv[1]
report = Report(EV)
for path in sorted(Path(__file__).with_name("checks").glob("[0-9]*.py")):
    import_module(f"checks.{path.stem}").judge(report)
rows = report.rows

count = {r: sum(1 for x in rows if x['result'] == r) for r in ('pass', 'fail', 'not-run')}
with open(report.directory / 'adapter.json', 'w') as f:
    json.dump({'evidence_dir': EV, 'counts': count, 'rows': rows}, f, indent=2)
lines = [f"{'Prop':<6} {'Result':<8} Check", '-' * 78]
for r in rows:
    lines.append(f"{r['property']:<6} {r['result']:<8} {r['check']}: {r['what']}")
    lines.append(f"{'':<15} {r['note']}")
lines += ['-' * 78, f"adapter suite: {count['pass']} pass, {count['fail']} fail, {count['not-run']} not-run"]
text = '\n'.join(lines) + '\n'
with open(report.directory / 'adapter.txt', 'w') as f:
    f.write(text)
sys.stdout.write(text)
sys.exit(1 if count['fail'] else 0)
