"""Secret redaction and macOS scenario applicability regressions."""

import json
import unittest

from features import out_of_scope
from harness import redact_osk


class MacosTests(unittest.TestCase):
    def test_description_redacts_escaped_keys_in_mixed_logs(self):
        key = ('FAKE"\\ ' * 8 + 'FAKEFAKE')
        self.assertEqual(len(key), 64)
        description = {"name": "test", "applesmc": {"osk": key}}
        for dump in [json.dumps(description), json.dumps(description, indent=2)]:
            log = redact_osk("machine: " + dump + "\ncontinued\n")
            self.assertNotIn(key, log)
            self.assertNotIn(json.dumps(key)[1:-1], log)
            self.assertIn('"osk": "[redacted]"', log)
            self.assertTrue(log.endswith("\ncontinued\n"))

    def test_macos_runs_only_on_its_delivered_backend(self):
        self.assertIsNone(out_of_scope("macos.ready", "vmlab"))
        self.assertIsNotNone(out_of_scope("macos.ready", "qemu"))
        self.assertIsNotNone(out_of_scope("macos.ready", "vmlab", "2f"))


if __name__ == "__main__":
    unittest.main()
