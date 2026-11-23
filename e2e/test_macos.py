"""The macOS scenario's host-config key insertion and its scope."""

import unittest

from features import NO_X86_ON_QEMU, out_of_scope
from harness import ScenarioFailed
from scenarios.macos import FEATURE, with_key

KEY = "k" * 63 + '"'


class MacosScenario(unittest.TestCase):
    def test_the_key_lands_escaped_inside_the_host_block(self):
        config = 'import <vmlab-host.wcl>\n\nhost {\n  hypervisor { container = "vmlab" vm = "vmlab" }\n}\n'
        got = with_key(config, KEY)
        self.assertIn('host {\n  apple_smc_osk = "' + "k" * 63 + '\\""\n  hypervisor', got)

    def test_a_key_of_the_wrong_shape_is_refused_without_echoing_it(self):
        for key in ("short", "k" * 63 + "\n", "k" * 65):
            with self.assertRaises(ScenarioFailed) as raised:
                with_key("host {\n}\n", key)
            self.assertNotIn(key.strip(), str(raised.exception))

    def test_macos_runs_on_vmlab_hv_only(self):
        self.assertIsNone(out_of_scope(FEATURE, "vmlab"))
        self.assertEqual(out_of_scope(FEATURE, "qemu"), NO_X86_ON_QEMU)


if __name__ == "__main__":
    unittest.main()
