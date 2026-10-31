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

    def test_counts_an_isolated_hypervisor_verification_binary(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            process = root / "100"
            process.mkdir()
            (process / "cmdline").write_bytes(b"/work/target/debug/vmlab-hv-test-plain\0")
            self.assertEqual(project_vms(root), [100])

    def test_counts_a_rust_test_only_when_it_owns_a_kvm_vm(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            for pid in [100, 101, 102]:
                process = root / str(pid)
                process.mkdir()
                (process / "cmdline").write_bytes(b"/work/target/debug/vmlab_hv-test\0")
                (process / "fd").mkdir()
            (root / "100/fd/4").symlink_to("anon_inode:kvm-vm")
            (root / "100/fd/5").symlink_to("anon_inode:kvm-vm")
            (root / "101/fd/4").symlink_to("anon_inode:kvm-vcpu")
            (root / "102/fd/4").symlink_to("anon_inode:kvm-vm")
            (root / "102/cmdline").write_bytes(b"qemu-system-x86_64\0-avd\0owner\0")
            self.assertEqual(project_vms(root), [100])


if __name__ == "__main__":
    unittest.main()
