"""Checks that the log shows how long each command took, and that a timeout
says how loaded the host was."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import harness


class HostLoadTests(unittest.TestCase):
    def test_load_and_io_pressure_in_one_line(self):
        with tempfile.TemporaryDirectory() as d:
            proc = Path(d)
            (proc / "pressure").mkdir()
            (proc / "loadavg").write_text("19.20 24.01 28.33 3/900 4242\n")
            (proc / "pressure" / "io").write_text(
                "some avg10=95.00 avg60=93.10 avg300=80.00 total=1\n"
                "full avg10=91.00 avg60=89.50 avg300=70.00 total=1\n"
            )
            got = harness.host_load(proc)
        self.assertEqual(got, "load 19.20 24.01 28.33, io some avg60=93.10%, io full avg60=89.50%")

    def test_missing_files_are_left_out(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(harness.host_load(Path(d)), "load unknown")


class RunTimingTests(unittest.TestCase):
    def harness(self, root):
        with patch.object(harness, "RESULTS", root / "results"), patch.object(harness, "WORK", root / "work"):
            h = harness.Harness(hypervisor="qemu")
        self.addCleanup(h.log.close)
        return h

    def test_exit_line_carries_the_elapsed_time(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            h = self.harness(root)
            h.run(["true"], cwd=root)
            h.log.flush()
            log = (root / "results" / "e2e.log").read_text()
        self.assertRegex(log, r"\[exit 0 after \d+\.\ds\]")

    def test_timeout_reports_the_host_load(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            h = self.harness(root)
            with patch.object(harness, "host_load", return_value="load 1.00 2.00 3.00"):
                r = h.run(["sleep", "5"], cwd=root, timeout=0.2, check=False)
            h.log.flush()
            log = (root / "results" / "e2e.log").read_text()
        self.assertEqual(r.code, 124)
        self.assertIn("[timed out after 0.2s; host load 1.00 2.00 3.00]", r.err)
        self.assertRegex(log, r"\[exit 124 after 0\.\ds\]")


if __name__ == "__main__":
    unittest.main()
