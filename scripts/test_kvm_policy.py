"""Exercise the real Cargo KVM probe with /dev/kvm hidden by bubblewrap."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class KvmPolicy(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.directory.cleanup)
        root = Path(__file__).resolve().parents[1]
        source = Path(cls.directory.name) / "probe.rs"
        source.write_text(
            f'include!("{root / "build/kvm-tests.rs"}");\n'
            'fn main() { configure(); }\n'
        )
        cls.binary = Path(cls.directory.name) / "probe"
        subprocess.run(["rustc", str(source), "-o", str(cls.binary)], check=True)

    def probe(self, required):
        return subprocess.run(
            ["bwrap", "--ro-bind", "/", "/", "--dev", "/dev", str(self.binary)],
            env={**os.environ, "VMLAB_REQUIRE_KVM": required},
            capture_output=True,
            text=True,
        )

    def test_absent_kvm_is_ignored_with_reason(self):
        result = self.probe("0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo:rustc-cfg=vmlab_ci_no_kvm", result.stdout)
        self.assertIn("KVM tests ignored: /dev/kvm is absent", result.stdout)

    def test_required_kvm_fails(self):
        result = self.probe("1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("VMLAB_REQUIRE_KVM=1 but /dev/kvm is absent", result.stderr)
        self.assertNotIn("cargo:rustc-cfg=vmlab_ci_no_kvm", result.stdout)
