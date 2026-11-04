"""Exercise guest release assembly without publishing or building drivers."""

import importlib.util
import json
import re
from pathlib import Path
import subprocess
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("assemble", ROOT / "scripts/release/assemble.py")
assemble = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(assemble)


class ReleaseTest(unittest.TestCase):
    def test_signing_openssl_pin_matches_the_documented_input(self):
        dockerfile = (ROOT / "build/signing/Dockerfile").read_text()
        pins = (ROOT / "build/signing/PINS").read_text()
        version = re.search(r"openssl=([^\s\\]+)", dockerfile).group(1)
        self.assertIn(f"openssl {version},", pins)
        self.assertNotEqual(version, "3.0.22-1~deb12u1")

    def test_patch_table_ignores_empty_directories(self):
        spec = importlib.util.spec_from_file_location("pins", ROOT / "scripts/pins-table.py")
        pins = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(pins)
        with tempfile.TemporaryDirectory() as temporary:
            pins.REPO = Path(temporary)
            (pins.REPO / "patches/dxil-spirv").mkdir(parents=True)
            (pins.REPO / "patches/dxil-spirv/README.md").write_text("No local patches.\n")
            original = pins.patches_table()
            (pins.REPO / "patches/host/virglrenderer/diagnostic").mkdir(parents=True)
            self.assertEqual(pins.patches_table(), original)
            self.assertTrue(any("dxil-spirv" in row for row in original))
            self.assertFalse(any("host" in row for row in original))

    def test_dry_run_cannot_publish(self):
        result = subprocess.run(["bash", str(ROOT / "scripts/release.sh"),
                                 "--dry-run", "--publish"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("cannot publish", result.stderr)

    def test_licence_sources_have_texts(self):
        sources = json.loads((ROOT / "licenses/SOURCES.json").read_text())
        self.assertEqual(len(sources), 8)
        for source in sources:
            self.assertGreater((ROOT / "licenses" / source["name"]).stat().st_size, 700)

    def test_zip_is_repeatable_and_contains_licences(self):
        with tempfile.TemporaryDirectory() as temporary:
            first, second = Path(temporary) / "first.zip", Path(temporary) / "second.zip"
            assemble.make_zip(ROOT / "licenses", first, 1789748904)
            assemble.make_zip(ROOT / "licenses", second, 1789748904)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            with zipfile.ZipFile(first) as archive:
                self.assertIn("MPL-2.0.txt", archive.namelist())
                self.assertIn("vkd3d-proton-LGPL-2.1.txt", archive.namelist())

    def test_tampered_prebuilt_package_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            package = Path(temporary) / "package"
            package.mkdir()
            cert = package / "vmlab-win3d-test.cer"
            cert.write_bytes(b"public certificate test data")
            payload = package / "driver.sys"
            payload.write_bytes(b"original payload")
            manifest = {"version": "0.1.2", "files": [{"path": "driver.sys",
                        "size": payload.stat().st_size, "sha256": assemble.sha256(payload)}]}
            (package / "manifest.json").write_text(json.dumps(manifest))
            payload.write_bytes(b"tampered payload")
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                assemble.guest(package, Path(temporary) / "output", "0.1.2", "source", cert)


if __name__ == "__main__":
    unittest.main()
