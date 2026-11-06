"""Checks for the JUnit file a run writes beside results.json."""

import contextlib
import io
import json
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
from unittest.mock import patch

import harness


def row(feature, status, detail=""):
    return {"feature": feature, "description": "", "hypervisor": "qemu", "status": status, "detail": detail}


class JunitTests(unittest.TestCase):
    def test_each_status_maps_to_its_junit_outcome(self):
        rows = [row("a", "pass"), row("b", "fail", "boom"), row("c", "missing", "no scenario exercised it"),
                row("d", "skip", "vmlab-hv phase 3")]
        root = ET.fromstring(harness.junit_xml(rows))
        suite = root.find("testsuite")
        self.assertEqual(
            {k: suite.get(k) for k in ("tests", "failures", "errors", "skipped")},
            {"tests": "4", "failures": "2", "errors": "0", "skipped": "1"},
        )
        cases = {c.get("name"): c for c in suite.findall("testcase")}
        self.assertEqual(list(cases["a"]), [])
        self.assertEqual(cases["b"].find("failure").get("type"), "fail")
        self.assertEqual(cases["b"].find("failure").get("message"), "boom")
        self.assertEqual(cases["c"].find("failure").get("type"), "missing")
        self.assertEqual(cases["d"].find("skipped").get("message"), "vmlab-hv phase 3")
        self.assertEqual(cases["a"].get("classname"), "e2e.qemu")

    def test_control_characters_in_a_detail_keep_the_xml_valid(self):
        rows = [row("a", "fail", "\x1b[31mred\x1b[0m\x00 end\tok"), row("b", "skip", "nul\x00")]
        cases = {c.get("name"): c for c in ET.fromstring(harness.junit_xml(rows)).find("testsuite")}
        failure = cases["a"].find("failure")
        self.assertEqual(failure.get("message"), "?[31mred?[0m? end\tok")
        self.assertEqual(failure.text, "?[31mred?[0m? end\tok")
        self.assertEqual(cases["b"].find("skipped").get("message"), "nul?")

    def test_report_writes_junit_with_the_counts_of_results_json(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            with patch.object(harness, "RESULTS", root / "results"), patch.object(harness, "WORK", root / "work"):
                h = harness.Harness(hypervisor="qemu")
                self.addCleanup(h.log.close)
                first = next(iter(harness.FEATURES))
                h.ok(first, True)
                with contextlib.redirect_stdout(io.StringIO()):
                    h.report()
                rows = json.loads((root / "results" / "results.json").read_text())
                suite = ET.parse(root / "results" / "junit.xml").getroot().find("testsuite")
        self.assertEqual(int(suite.get("tests")), len(rows))
        self.assertEqual(int(suite.get("failures")), sum(r["status"] in ("fail", "missing") for r in rows))
        self.assertEqual(int(suite.get("skipped")), sum(r["status"] == "skip" for r in rows))
        self.assertEqual(len(suite.findall("testcase")), len(rows))


if __name__ == "__main__":
    unittest.main()
