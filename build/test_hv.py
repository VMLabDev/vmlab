"""Exercise the release desktop payload and the real per-user installer."""

import configparser
import hashlib
import os
import json
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
from unittest import mock

import hv


class ViewerDesktopTest(unittest.TestCase):
    def test_external_ci_output_paths(self):
        probe = """
import json, runpy
hv = runpy.run_path('build/hv.py')
ipxe = runpy.run_path('guest/ipxe/build.py')
print(json.dumps([str(hv['CACHE']), str(ipxe['PROFILES']['hv']['output']),
                  str(ipxe['PROFILES']['hv']['source'])]))
"""
        for root in ("", "/tmp/vmlab-external-output"):
            with self.subTest(root=root):
                env = dict(os.environ, VMLAB_TARGET_ROOT=root, PYTHONDONTWRITEBYTECODE="1")
                paths = json.loads(subprocess.check_output(
                    ["python3", "-c", probe], cwd=hv.ROOT, env=env, text=True))
                base = Path(root) if root else hv.ROOT
                output = base / "ipxe" if root else base / "hv/target/ipxe"
                self.assertEqual(paths, [str(base / ".ci-cache"), str(output),
                                         str(output / "upstream")])

    def test_virgl_cache_key_covers_applied_patches(self):
        self.assertEqual(len(hv.PATCHES), 3)
        expected = hashlib.sha256(b"".join(
            patch.name.encode() + b"\0" + patch.read_bytes() for patch in hv.PATCHES)).hexdigest()
        self.assertIn(expected, hv.CACHE_KEY)
        self.assertTrue(any(patch.name.startswith("0002-") for patch in hv.PATCHES))
        self.assertTrue(any(patch.name.startswith("0003-") for patch in hv.PATCHES))

    def test_virgl_cache_key_changes_with_the_pin_and_the_patch_set(self):
        key = hv.cache_key(hv.VIRGL_VERSION, hv.VIRGL_SHA256, hv.PATCHES, hv.VIRGL_MESON_OPTIONS)
        self.assertEqual(key, hv.CACHE_KEY)
        self.assertNotEqual(hv.cache_key("9.9.9", hv.VIRGL_SHA256, hv.PATCHES, hv.VIRGL_MESON_OPTIONS), key)
        self.assertNotEqual(hv.cache_key(hv.VIRGL_VERSION, "0" * 64, hv.PATCHES, hv.VIRGL_MESON_OPTIONS), key)
        self.assertNotEqual(hv.cache_key(hv.VIRGL_VERSION, hv.VIRGL_SHA256, hv.PATCHES[:-1], hv.VIRGL_MESON_OPTIONS), key)
        with tempfile.TemporaryDirectory() as temporary:
            edited = Path(temporary) / hv.PATCHES[-1].name
            edited.write_bytes(hv.PATCHES[-1].read_bytes() + b"\n")
            self.assertNotEqual(
                hv.cache_key(hv.VIRGL_VERSION, hv.VIRGL_SHA256, [*hv.PATCHES[:-1], edited], hv.VIRGL_MESON_OPTIONS), key)

    def test_virgl_cache_key_changes_with_a_meson_option(self):
        """Review install-release-2 F13: a commit that changes one meson option
        must not get the libraries of the earlier build."""
        key = hv.cache_key(hv.VIRGL_VERSION, hv.VIRGL_SHA256, hv.PATCHES, hv.VIRGL_MESON_OPTIONS)
        self.assertEqual(key, hv.CACHE_KEY)
        changed = [*hv.VIRGL_MESON_OPTIONS[:-1], hv.VIRGL_MESON_OPTIONS[-1] + "x"]
        for options in (changed, hv.VIRGL_MESON_OPTIONS[:-1], [*hv.VIRGL_MESON_OPTIONS, "-Dextra=true"]):
            self.assertNotEqual(
                hv.cache_key(hv.VIRGL_VERSION, hv.VIRGL_SHA256, hv.PATCHES, options), key, options)

    def virgl_cache(self, root):
        """`hv` with its cache under `root`, pkg-config stubbed, and a
        `virgl_install` that writes a small tree and counts its calls."""
        cache = Path(root) / ".ci-cache"
        installs = []

        def install(prefix, scratch=None):
            installs.append(Path(prefix))
            for name, text in (("lib/pkgconfig/virglrenderer.pc", f"Version: {hv.VIRGL_VERSION}\n"),
                               ("lib/libvirglrenderer.so.1", "built library\n"),
                               ("libexec/virgl_render_server", "built server\n")):
                path = Path(prefix) / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)

        patches = (mock.patch.object(hv, "CACHE", cache),
                   mock.patch.object(hv, "VIRGL", cache / hv.CACHE_KEY),
                   mock.patch.object(hv, "DIST_VIRGL", Path(root) / "target/hv-virgl"),
                   mock.patch.object(hv, "run"),
                   mock.patch.object(hv, "virgl_install", side_effect=install))
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)
        return cache / hv.CACHE_KEY, installs

    def test_a_planted_virgl_cache_is_rebuilt(self):
        """Review install-release-2 F13: a directory with the right name, a
        `.complete` and the pinned version in its .pc file proves nothing about
        the binaries in it."""
        with tempfile.TemporaryDirectory() as root:
            virgl, installs = self.virgl_cache(root)
            (virgl / "lib/pkgconfig").mkdir(parents=True)
            (virgl / "lib/pkgconfig/virglrenderer.pc").write_text(f"Version: {hv.VIRGL_VERSION}\n")
            (virgl / "libexec").mkdir()
            server = virgl / "libexec/virgl_render_server"
            server.write_text("planted\n")
            (virgl / "lib/planted.so").write_text("planted\n")
            (virgl / ".complete").touch()
            hv.virgl_build()
            self.assertEqual(installs, [virgl])
            self.assertEqual(server.read_text(), "built server\n")
            self.assertFalse((virgl / "lib/planted.so").exists())
            # The tree that this build left is reused as it is.
            hv.virgl_build()
            self.assertEqual(installs, [virgl])
            # One changed, added or removed file, and it is built again.
            for change in (lambda: server.write_text("planted\n"),
                           lambda: (virgl / "lib/libvirglrenderer.so.2").write_text("planted\n"),
                           lambda: (virgl / "lib/libvirglrenderer.so.1").unlink()):
                change()
                built = len(installs)
                hv.virgl_build()
                self.assertEqual(len(installs), built + 1)
                self.assertEqual(server.read_text(), "built server\n")
                self.assertEqual(sorted(path.name for path in (virgl / "lib").iterdir()),
                                 ["libvirglrenderer.so.1", "pkgconfig"])

    def test_a_dist_build_takes_no_virgl_from_the_cache(self):
        """Review install-release-2 F13: who can write the cache can write a
        tree that agrees with its own record, so the bundle of a release is
        packaged from a virglrenderer built in that run, outside the cache."""
        with tempfile.TemporaryDirectory() as root:
            virgl, installs = self.virgl_cache(root)
            hv.virgl_build()
            (virgl / "libexec/virgl_render_server").write_text("planted\n")
            (virgl / ".complete").write_text(hv.tree_manifest(virgl))
            with mock.patch.dict(os.environ, {"VMLAB_HV_VIRGL": "fresh"}):
                # `virgl-build` of the recipe builds nothing into the cache.
                hv.virgl_build()
                self.assertEqual(installs, [virgl])
                # What an earlier run left where the dist build works is not kept.
                left = hv.DIST_VIRGL / "prefix/lib/libvirglrenderer.so.9"
                left.parent.mkdir(parents=True)
                left.write_text("planted\n")
                with hv.package_virgl() as prefix:
                    self.assertEqual(installs, [virgl, prefix])
                    self.assertNotIn(hv.CACHE, prefix.parents)
                    self.assertEqual((prefix / "libexec/virgl_render_server").read_text(),
                                     "built server\n")
                    self.assertFalse(left.exists())
                self.assertFalse(hv.DIST_VIRGL.exists())
            # Outside a dist build the cache is the tree to package.
            with hv.package_virgl() as prefix:
                self.assertEqual(prefix, virgl)
            self.assertEqual(len(installs), 2)

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

    def test_test_native_deps_install_runs_every_step(self):
        # VML-48: the steps after the apt install lost their sudo prefix
        # and raised NameError. Run it with every host effect stubbed.
        calls = []
        with mock.patch.object(hv, "run", side_effect=lambda *a, **k: calls.append([str(x) for x in a])), \
                mock.patch.object(hv.os, "geteuid", return_value=1000), \
                mock.patch.object(hv.shutil, "which", return_value=None), \
                mock.patch.object(hv.Path, "exists", return_value=False):
            hv.test_native_deps_install()
        self.assertEqual(calls[0], ["sudo", "apt-get", "update"])
        self.assertEqual(calls[1][:5], ["sudo", "apt-get", "install", "-y", "--no-install-recommends"])
        self.assertEqual(calls[1][5:], hv.native_packages("virgl", "hypervisor", "test"))
        self.assertEqual(calls[2:], [
            ["sudo", "install", "-d", "/usr/share/qemu"],
            ["sudo", "ln", "-s", "/usr/share/seabios/vgabios-stdvga.bin",
             "/usr/share/qemu/vgabios-stdvga.bin"],
            ["sudo", "ln", "-s", "/usr/share/seabios/vgabios-virtio.bin",
             "/usr/share/qemu/vgabios-virtio.bin"],
            ["sudo", "ln", "-s", "/usr/bin/convert", "/usr/local/bin/magick"],
        ])

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
