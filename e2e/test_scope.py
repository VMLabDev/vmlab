"""Regression checks for scenario routing and per-feature scope accounting."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import harness
from features import HOST, PHASES
from run import scenario_features


class ScenarioScopeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        for name in ("WORK", "RESULTS"):
            replacement = patch.object(harness, name, root / name.lower())
            replacement.start()
            self.addCleanup(replacement.stop)

    def new_harness(self, hypervisor):
        h = harness.Harness(hypervisor=hypervisor)
        self.addCleanup(h.log.close)
        return h

    def test_all_33_delivered_core_and_admin_features_follow_vmlab(self):
        h = self.new_harness("vmlab")
        delivered = set()
        for name in ("core", "admin"):
            h.scenario = name
            features = scenario_features(name)
            h.vms_here = h.vms_follow(features, name)
            self.assertTrue(h.vms_here, name)
            for feature in features:
                if PHASES.get(feature) == HOST or h.out_of_scope(feature):
                    continue
                delivered.add(feature)
                h.ok(feature, True, "scope regression")
                self.assertEqual(h.results[feature][0], "pass", feature)
        self.assertEqual(len(delivered), 33)

    def test_host_config_pins_both_kinds_for_every_run(self):
        for hypervisor in ("qemu", "vmlab"):
            h = self.new_harness(hypervisor)
            for vms in (False, True):
                config = h.host_config(vms)
                self.assertIn(f'container = "{hypervisor}"', config)
                vm = hypervisor if vms else "qemu"
                self.assertIn(f'vm = "{vm}"', config)

    def test_unsupported_features_keep_their_reasons(self):
        h = self.new_harness("vmlab")
        h.vms_here = True
        for feature, reason in {
            "vm.hw.qemu_args": "QEMU only:",
            "vm.hw.floppy": "vmlab-hv phase 4",
            "vm.media.floppy": "vmlab-hv phase 4",
            "arch.emulated": "vmlab-hv phase 3",
        }.items():
            with self.subTest(feature=feature):
                h.ok(feature, True)
                status, detail = h.results[feature]
                self.assertEqual(status, "skip")
                self.assertTrue(detail.startswith(reason), detail)

    def test_qemu_keeps_every_core_and_admin_feature_in_scope(self):
        h = self.new_harness("qemu")
        for name in ("core", "admin"):
            h.scenario = name
            for feature in scenario_features(name):
                with self.subTest(feature=feature):
                    self.assertIsNone(h.out_of_scope(feature))
                    h.ok(feature, True)
                    self.assertEqual(h.results[feature][0], "pass")

    def test_fastpath_stays_on_qemu_and_cannot_overwrite_a_failure(self):
        h = self.new_harness("vmlab")
        feature = "net.fastpath.kernel"
        self.assertFalse(h.vms_follow({feature}, "fastpath"))
        h.scenario = "fastpath"
        h.ok(feature, True)
        self.assertEqual(h.results[feature][0], "skip")
        h.scenario, h.vms_here = "core", True
        h.ok("lab.destroy", False, "real failure")
        h.scenario = "admin"
        h.ok("lab.destroy", True)
        self.assertEqual(h.results["lab.destroy"], ("fail", "real failure"))


if __name__ == "__main__":
    unittest.main()
