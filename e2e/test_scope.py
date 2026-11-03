"""Regression checks for scenario routing and per-feature scope accounting."""

import tempfile
import unittest
import json
from pathlib import Path
from unittest.mock import patch

import harness
from features import HOST, PHASES
from run import scenario_features
from scenarios.fastpath import nic_attachments


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

    def test_all_34_delivered_core_and_admin_features_follow_vmlab(self):
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
        self.assertIn("vm.hw.tsc", delivered)
        self.assertEqual(len(delivered), 34)

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

    def test_fastpath_follows_vmlab_and_cannot_overwrite_a_failure(self):
        h = self.new_harness("vmlab")
        feature = "net.fastpath.kernel"
        self.assertTrue(h.vms_follow({feature}, "fastpath"))
        h.scenario = "fastpath"
        h.vms_here = True
        h.ok(feature, True)
        self.assertEqual(h.results[feature][0], "pass")
        h.scenario, h.vms_here = "core", True
        h.ok("lab.destroy", False, "real failure")
        h.scenario = "admin"
        h.ok("lab.destroy", True)
        self.assertEqual(h.results["lab.destroy"], ("fail", "real failure"))


class NicAttachmentTests(unittest.TestCase):
    def test_backend_neutral_attachment_inspection_checks_open_tap_fds(self):
        for hypervisor in ("qemu", "vmlab"):
            with self.subTest(hypervisor=hypervisor), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                proc, runtime = root / "proc", root / "runtime"
                proc.mkdir()
                for index, machine in enumerate(("fp01", "fp02"), 1):
                    process = proc / str(index)
                    (process / "fd").mkdir(parents=True)
                    if hypervisor == "vmlab":
                        description = runtime / "lab" / "vms" / machine / "machine.json"
                        description.parent.mkdir(parents=True)
                        description.write_text(json.dumps({"nics": [{"attachment": {"kind": "tap_raw", "queue_pairs": 2}}]}))
                        argv = ["vmlab-hv", "--machine", str(description)]
                        pairs = 2
                    else:
                        argv = ["qemu-system-x86_64", "-name", f"vmlab:e2e-unsupported-fastpath/{machine}", "-netdev", "tap,id=net0,fd=10"]
                        pairs = 1
                    (process / "cmdline").write_bytes("\0".join(argv).encode())
                    for fd in range(pairs):
                        (process / "fd" / str(fd + 10)).symlink_to("/dev/net/tun")
                pairs = 2 if hypervisor == "vmlab" else 1
                kind = "tap_raw" if hypervisor == "vmlab" else "tap"
                self.assertEqual(nic_attachments(hypervisor, runtime, proc), [(kind, pairs, pairs)] * 2)


if __name__ == "__main__":
    unittest.main()
