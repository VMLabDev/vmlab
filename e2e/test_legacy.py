"""Host-wide VM counting for the legacy gate's shared-host limit."""

import pathlib
import tempfile
import unittest

from scenarios.legacy import project_vms


class ProjectVmCount(unittest.TestCase):
    def test_counts_argv_even_when_the_executable_has_been_replaced(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            for pid, argv in {
                100: ["/work/target/release/vmlab-hv", "--machine", "/tmp/machine.json"],
                101: ["qemu-system-x86_64", "-machine", "pc"],
                102: ["qemu-system-x86_64", "-avd", "owners-emulator"],
                103: ["bash", "-c", "vmlab-hv --machine some.json"],
            }.items():
                process = root / str(pid)
                process.mkdir()
                (process / "cmdline").write_bytes(b"\0".join(arg.encode() for arg in argv) + b"\0")
                (process / "exe").symlink_to("/old/vmlab-hv (deleted)")
            self.assertEqual(sorted(project_vms(root)), [100, 101])

    def test_ignores_processes_that_have_exited(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "100").mkdir()
            self.assertEqual(project_vms(root), [])


if __name__ == "__main__":
    unittest.main()
