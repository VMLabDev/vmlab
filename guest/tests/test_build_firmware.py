"""Check firmware routing without downloading sources or installing assets."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class FirmwareBuildTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vmlab-build-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name) / "checkout with spaces"
        self.root.mkdir()
        for name in ("guest", "guest/ipxe", "build", "bin"):
            (self.root / name).mkdir(exist_ok=True)
        self.log = self.root / "calls"
        self.env = dict(os.environ, BUILD_TEST_LOG=str(self.log))
        self.env.pop("VMLAB_BUILD_ENV", None)
        self.env.pop("VMLAB_BUILD_IMAGE", None)
        shutil.copy(ROOT / "guest/build-firmware.sh", self.root / "guest")
        for name in ("fetch-firmware.sh", "build-ovmf.sh", "build-seabios.sh"):
            self.script(
                "guest/" + name,
                'printf "%s\\n" "$(basename "$0") $*" >> "$BUILD_TEST_LOG"\n',
            )
        (self.root / "guest/ipxe/build.py").write_text(
            'import os\nwith open(os.environ["BUILD_TEST_LOG"], "a") as log:\n'
            '    log.write("ipxe\\n")\n'
        )
        self.script(
            "build/run.sh",
            'echo container >> "$BUILD_TEST_LOG"\n'
            'export VMLAB_BUILD_ENV=1\nexec "$@"\n',
        )

    def script(self, name, body):
        path = self.root / name
        path.write_text("#!/usr/bin/env bash\nset -euo pipefail\n" + body)
        path.chmod(0o755)

    def run_firmware(self, *arches):
        return subprocess.run(
            ["bash", str(self.root / "guest/build-firmware.sh"), *arches],
            env=self.env,
            capture_output=True,
            text=True,
            check=True,
        )

    def calls(self):
        return self.log.read_text().splitlines()

    def test_host_enters_container_once_and_preserves_arches(self):
        self.run_firmware("aarch64", "x86_64")
        self.assertEqual(self.calls(), [
            "container", "fetch-firmware.sh aarch64 x86_64",
            "build-ovmf.sh ", "ipxe", "build-seabios.sh ",
        ])

    def test_provisioned_environment_does_not_nest_container(self):
        self.env["VMLAB_BUILD_ENV"] = "1"
        self.run_firmware()
        self.assertEqual(self.calls(), [
            "fetch-firmware.sh x86_64 aarch64",
            "build-ovmf.sh ", "ipxe", "build-seabios.sh ",
        ])

    def test_aarch64_does_not_build_x86_firmware(self):
        self.run_firmware("aarch64")
        self.assertEqual(self.calls(), ["container", "fetch-firmware.sh aarch64"])

    def test_failure_stops_remaining_firmware_builds(self):
        self.script("guest/build-ovmf.sh", "exit 23\n")
        with self.assertRaises(subprocess.CalledProcessError) as result:
            self.run_firmware("x86_64")
        self.assertEqual(result.exception.returncode, 23)
        self.assertEqual(self.calls(), ["container", "fetch-firmware.sh x86_64"])

    def run_container(self, *, missing=False, build_failure=False):
        shutil.copy(ROOT / "build/run.sh", self.root / "build")
        docker = self.root / "bin/docker"
        docker.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            'with open(os.environ["BUILD_TEST_LOG"], "a") as log:\n'
            '    log.write(json.dumps(sys.argv[1:]) + "\\n")\n'
            'if sys.argv[1:3] == ["image", "inspect"]:\n'
            '    sys.exit(int(os.environ["TEST_IMAGE_MISSING"]))\n'
            'if sys.argv[1] == "build":\n'
            '    sys.exit(int(os.environ["TEST_BUILD_FAILURE"]))\n'
        )
        docker.chmod(0o755)
        self.env.update(
            PATH=str(self.root / "bin") + os.pathsep + os.environ["PATH"],
            VMLAB_BUILD_IMAGE="custom-build:test",
            TEST_IMAGE_MISSING=str(int(missing)),
            TEST_BUILD_FAILURE="17" if build_failure else "0",
        )
        result = subprocess.run(
            [str(self.root / "build/run.sh"), "bash", "-c", "exit 0"],
            env=self.env, capture_output=True, text=True,
        )
        return result, [json.loads(line) for line in self.calls()]

    def test_existing_custom_image_is_reused_with_environment_marker(self):
        result, calls = self.run_container()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0], ["image", "inspect", "custom-build:test"])
        self.assertEqual([call[0] for call in calls], ["image", "run"])
        self.assertIn("VMLAB_BUILD_ENV=1", calls[1])
        self.assertIn("custom-build:test", calls[1])

    def test_missing_image_is_built_before_run(self):
        result, calls = self.run_container(missing=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[0] for call in calls], ["image", "build", "run"])
        self.assertEqual(calls[1], [
            "build", "-f", str(self.root / "build/Dockerfile"),
            "-t", "custom-build:test", str(self.root),
        ])

    def test_image_build_failure_prevents_run(self):
        result, calls = self.run_container(missing=True, build_failure=True)
        self.assertEqual(result.returncode, 17)
        self.assertEqual([call[0] for call in calls], ["image", "build"])


if __name__ == "__main__":
    unittest.main()
