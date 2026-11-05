"""Exercise the release desktop payload and the real per-user installer."""

import configparser
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest

import hv


class ViewerDesktopTest(unittest.TestCase):
    def test_virgl_cache_key_covers_applied_patches(self):
        self.assertEqual(len(hv.PATCHES), 3)
        expected = hashlib.sha256(b"".join(
            patch.name.encode() + b"\0" + patch.read_bytes() for patch in hv.PATCHES)).hexdigest()
        self.assertIn(expected, hv.CACHE_KEY)
        self.assertTrue(any(patch.name.startswith("0002-") for patch in hv.PATCHES))
        self.assertTrue(any(patch.name.startswith("0003-") for patch in hv.PATCHES))

    def test_virgl_cache_key_changes_with_the_pin_and_the_patch_set(self):
        key = hv.cache_key(hv.VIRGL_VERSION, hv.VIRGL_SHA256, hv.PATCHES)
        self.assertEqual(key, hv.CACHE_KEY)
        self.assertNotEqual(hv.cache_key("9.9.9", hv.VIRGL_SHA256, hv.PATCHES), key)
        self.assertNotEqual(hv.cache_key(hv.VIRGL_VERSION, "0" * 64, hv.PATCHES), key)
        self.assertNotEqual(hv.cache_key(hv.VIRGL_VERSION, hv.VIRGL_SHA256, hv.PATCHES[:-1]), key)
        with tempfile.TemporaryDirectory() as temporary:
            edited = Path(temporary) / hv.PATCHES[-1].name
            edited.write_bytes(hv.PATCHES[-1].read_bytes() + b"\n")
            self.assertNotEqual(
                hv.cache_key(hv.VIRGL_VERSION, hv.VIRGL_SHA256, [*hv.PATCHES[:-1], edited]), key)

    def test_the_pin_file_is_the_one_definition(self):
        pins = tomllib.loads((hv.ROOT / "build/hv-pins.toml").read_text())
        self.assertEqual(hv.VIRGL_VERSION, pins["virglrenderer"]["version"])
        self.assertEqual(hv.VIRGL_SHA256, pins["virglrenderer"]["sha256"])
        # Nothing else carries the pin.
        for consumer in ("e2e/Dockerfile", "build/Dockerfile", ".github/workflows/ci.yml"):
            text = (hv.ROOT / consumer).read_text()
            self.assertNotIn(hv.VIRGL_SHA256, text, consumer)
            self.assertNotIn(f"virglrenderer-{hv.VIRGL_VERSION}", text, consumer)
        # Each install names a package once, and both share the virgl group.
        for groups in (("virgl", "hypervisor", "release"), ("virgl", "hypervisor", "test")):
            packages = hv.native_packages(*groups)
            self.assertEqual(len(packages), len(set(packages)), groups)
            self.assertTrue(set(hv.native_packages("virgl")) <= set(packages), groups)

    def test_release_stamp_updates_the_root_package(self):
        manifest = hv.ROOT / "Cargo.toml"
        original = tomllib.loads(manifest.read_text())
        stamped = subprocess.check_output(
            ["sed", '0,/^version = ".*"$/s//version = "9.9.9"/', str(manifest)], text=True)
        parsed = tomllib.loads(stamped)
        self.assertEqual(parsed["package"]["version"], "9.9.9")
        self.assertEqual(parsed["workspace"]["package"], original["workspace"]["package"])

    def test_revision_is_the_monorepo_commit(self):
        expected = subprocess.check_output(
            ["git", "-C", str(hv.ROOT), "rev-parse", "HEAD"], text=True).strip()
        self.assertEqual(hv.revision(), expected)

    def test_payload_and_installer(self):
        with tempfile.TemporaryDirectory(prefix="viewer icons ") as temporary:
            root = Path(temporary)
            stage = root / "bundle"
            hv.stage_viewer_desktop(stage)
            entry = stage / "share/applications/dev.vmlab.viewer.desktop"
            desktop = configparser.ConfigParser()
            desktop.read(entry)
            self.assertEqual(desktop["Desktop Entry"]["Icon"], "dev.vmlab.viewer")
            self.assertEqual(desktop["Desktop Entry"]["StartupWMClass"], "dev.vmlab.viewer")
            self.assertEqual(desktop["Desktop Entry"]["NoDisplay"], "true")
            self.assertEqual(desktop["Desktop Entry"]["Exec"], "vmlab-hv-viewer")
            for size in (16, 24, 32, 48, 64, 128, 256):
                png = stage / f"share/icons/hicolor/{size}x{size}/apps/dev.vmlab.viewer.png"
                self.assertEqual(png.read_bytes(), (hv.ROOT / f"guest/logo/icon-{size}.png").read_bytes())
            for custom in (False, True):
                with self.subTest(custom_data_home=custom):
                    env = os.environ.copy()
                    env["HOME"] = str(root / "user home")
                    env.pop("XDG_DATA_HOME", None)
                    if custom:
                        env["XDG_DATA_HOME"] = str(root / "custom data")
                    data = Path(env.get("XDG_DATA_HOME", str(root / "user home/.local/share")))
                    for _ in range(2):
                        subprocess.run([stage / "install-viewer-icons.sh"], env=env, check=True)
                    files = list((stage / "share").rglob("*"))
                    self.assertEqual(sum(path.is_file() for path in files), 9)
                    for path in files:
                        if path.is_file():
                            installed = data / path.relative_to(stage / "share")
                            self.assertEqual(installed.read_bytes(), path.read_bytes())
                            self.assertEqual(installed.stat().st_mode & 0o777, 0o644)

    def test_incomplete_payload_makes_no_installation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = root / "bundle"
            hv.stage_viewer_desktop(stage)
            (stage / "share/icons/hicolor/256x256/apps/dev.vmlab.viewer.png").unlink()
            env = os.environ.copy()
            env["XDG_DATA_HOME"] = str(root / "data")
            result = subprocess.run([stage / "install-viewer-icons.sh"], env=env)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((root / "data").exists())


if __name__ == "__main__":
    unittest.main()
