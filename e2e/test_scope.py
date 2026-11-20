"""Regression checks for scenario routing and per-feature scope accounting."""

import tempfile
import unittest
import json
from pathlib import Path
from unittest.mock import patch

import harness
from features import HOST, NO_X86_ON_QEMU, PHASES
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

    def test_all_35_delivered_core_and_admin_features_follow_vmlab(self):
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
        self.assertIn("arch.emulated", delivered)
        self.assertEqual(len(delivered), 35)

    def test_host_config_leaves_a_vmlab_run_to_automatic_selection(self):
        for vms in (False, True):
            # QEMU runs no x86 guest: a vmlab run names no hypervisor at all.
            self.assertEqual(self.new_harness("vmlab").host_config(vms), "")
            config = self.new_harness("qemu").host_config(vms)
            self.assertIn('container = "qemu"', config)
            self.assertIn('vm = "qemu"', config)

    def test_unsupported_features_keep_their_reasons(self):
        h = self.new_harness("vmlab")
        h.vms_here = True
        for feature, reason in {
            "vm.hw.floppy": "vmlab-hv phase 4",
            "vm.media.floppy": "vmlab-hv phase 4",
        }.items():
            with self.subTest(feature=feature):
                h.ok(feature, True)
                status, detail = h.results[feature]
                self.assertEqual(status, "skip")
                self.assertTrue(detail.startswith(reason), detail)

    def test_qemu_runs_its_aarch64_guest_alone(self):
        h = self.new_harness("qemu")
        for name in ("core", "admin"):
            h.scenario = name
            for feature in scenario_features(name):
                with self.subTest(feature=feature):
                    h.ok(feature, True)
                    if feature == "arch.emulated":
                        self.assertIsNone(h.out_of_scope(feature))
                        self.assertEqual(h.results[feature][0], "pass")
                    else:
                        self.assertEqual(h.results[feature], ("skip", NO_X86_ON_QEMU))
        # And on a vmlab run the aarch64 guest is in scope too: automatic
        # selection puts it on QEMU.
        self.assertIsNone(self.new_harness("vmlab").out_of_scope("arch.emulated"))

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
