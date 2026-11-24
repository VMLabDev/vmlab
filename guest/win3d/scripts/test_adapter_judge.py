"""Exercise the adapter judge with Wine evidence as collected from the guest."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


JUDGE = Path(__file__).resolve().parents[1] / 'tests/e2e/adapter/judge.py'


class WineControl(unittest.TestCase):
    def judge(self, device_log, device_failures):
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            (evidence / 'wine.status').write_text('ran\n')
            (evidence / 'wine.programs').write_text('d3d9_device\nd3d9_d3d9ex\n')
            logs = {
                'device': (device_log, device_failures),
                'd3d9ex': ('d3d9ex.c:687: Test failed: Test 2: Got unexpected hr 0x8876086c.\n', 1),
            }
            for program, (log, failures) in logs.items():
                check = f'wine-d3d9_{program}'
                (evidence / f'{check}.failures.txt').write_text(log, encoding='utf-8-sig')
                (evidence / f'{check}.wine.json').write_text(json.dumps({
                    'completed': True, 'exit_code': failures, 'executed': 100,
                    'failures': failures, 'todo': 0, 'skipped': 0, 'duration_s': 10,
                }))
            proc = subprocess.run([sys.executable, str(JUDGE), directory], capture_output=True, text=True)
            report = json.loads((evidence / 'adapter.json').read_text())
            return proc.returncode, {row['check']: row for row in report['rows']}

    def test_control_failures_pass(self):
        log = (
            "device.c:4319: Test failed: Expected message 0x7e for window 0, but didn't receive it, i=0.\n"
            "device.c:4319: Test failed: Expected message 0x7e for window 0, but didn't receive it, i=1.\n"
            "device.c:5680: Tests skipped: Following tests require two adapters.\n"
        )
        code, rows = self.judge(log, 2)
        self.assertEqual(code, 0)
        self.assertEqual(rows['wine-d3d9_device']['result'], 'pass')
        self.assertEqual(rows['wine-d3d9_d3d9ex']['result'], 'pass')
        self.assertIn('all 2 failures match', rows['wine-d3d9_device']['note'])

    def test_extra_failure_fails(self):
        log = (
            'device.c:5184: Test failed: Expected device window extended style 0x108, got 0, i=0.\n'
            'device.c:3238: Test failed: Got unexpected hr 0x80004005.\n'
        )
        code, rows = self.judge(log, 2)
        self.assertEqual(code, 1)
        self.assertEqual(rows['wine-d3d9_device']['result'], 'fail')
        self.assertIn('outside the control list: device.c:3238', rows['wine-d3d9_device']['note'])

    def test_incomplete_log_fails(self):
        code, rows = self.judge('device.c:15291: Test failed: Got refcount 27.\n', 2)
        self.assertEqual(code, 1)
        self.assertIn('1 assertion lines for 2 reported failures', rows['wine-d3d9_device']['note'])
