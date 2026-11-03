"""Check the TSC sanity test against guest calibration and exact launches."""

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from scenarios import core


class PinnedTscTests(unittest.TestCase):
    def check_tsc(self, hypervisor, measured, applied=True):
        pin = 4_291_934_000
        launch_pin = pin if applied else pin + 1000
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            vm = root / ".vmlab/vms/vm01"
            vm.mkdir(parents=True)
            (vm / "tsc-hz").write_text(str(pin))
            runtime = root / "run"
            machine = runtime / "vmlab/labs" / core.LAB / "vms/vm01/machine.json"
            machine.parent.mkdir(parents=True)
            machine.write_text(json.dumps({"invtsc": True, "tsc_khz": launch_pin // 1000}))
            argv = ["qemu-system-x86_64", "-cpu", f"host,+invtsc,tsc-frequency={launch_pin}"]
            dmesg = f"tsc: Detected {measured / 1_000_000:.3f} MHz processor\n" if measured else ""
            h = Mock()
            with patch.object(core, "qemu_argv", return_value=argv if hypervisor == "qemu" else []), \
                    patch.object(core, "sh", return_value=SimpleNamespace(out=dmesg)), \
                    patch.dict(core.os.environ, {"XDG_RUNTIME_DIR": str(runtime)}):
                core.pinned_tsc(h, root)
            return h.ok.call_args.args[1]

    def test_guest_calibration_within_kvm_tolerance_passes(self):
        for hypervisor in ("qemu", "vmlab"):
            for measured in (4_291_934_000, 4_291_932_000, 4_292_934_000):
                with self.subTest(hypervisor=hypervisor, measured=measured):
                    self.assertTrue(self.check_tsc(hypervisor, measured))

    def test_missing_or_wrong_guest_frequency_fails(self):
        for hypervisor in ("qemu", "vmlab"):
            for measured in (0, 4_294_934_000):
                with self.subTest(hypervisor=hypervisor, measured=measured):
                    self.assertFalse(self.check_tsc(hypervisor, measured))

    def test_launch_must_match_the_pin_exactly(self):
        for hypervisor in ("qemu", "vmlab"):
            with self.subTest(hypervisor=hypervisor):
                self.assertFalse(self.check_tsc(hypervisor, 4_291_934_000, applied=False))


if __name__ == "__main__":
    unittest.main()
