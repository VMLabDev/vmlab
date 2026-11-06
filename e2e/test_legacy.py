"""Host-wide VM counting for the legacy gate's shared-host limit."""

import pathlib
import tempfile
import unittest

from scenarios.legacy import GUESTS, SCREEN_MARKERS, lab_events, project_vms, screen_marker


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


class LabEvents(unittest.TestCase):
    def test_keeps_json_events_and_skips_plain_log_lines(self):
        out = "\n".join([
            "==> /state/labs/l/vms/freedos/qemu.log <==",
            "vmlab: argv: qemu-system-x86_64 -machine pc",
            '{"event": "vm.stopped", "data": {"vm": "freedos", "status": "exit status: 0"}}',
            "[exit 0]",
            "",
            "[1, 2]",
        ])
        self.assertEqual(
            lab_events(out),
            [{"event": "vm.stopped", "data": {"vm": "freedos", "status": "exit status: 0"}}],
        )


class ScreenMarker(unittest.TestCase):
    def test_every_guest_has_a_marker(self):
        self.assertEqual(sorted(SCREEN_MARKERS), sorted(m for m, _, _ in GUESTS))

    def test_windows_7_setup_is_not_its_usual_screen(self):
        self.assertIsNone(screen_marker("win7", "Setup is installing devices"))
        self.assertIsNone(screen_marker("win7", "Recycle Bin\nSetup is starting services"))

    def test_windows_7_desktop_matches_across_lines_and_case(self):
        self.assertEqual(screen_marker("win7", "RECYCLE\n  bin\n"), "recycle bin")

    def test_each_vml_50_screen_matches_its_own_guest_only(self):
        screens = {
            "freedos": "Welcome to the FreeDOS 1.3 operating system (http://www.freedos.org)",
            "win98": "Enter Windows Password\nType a name to identify yourself to Windows.",
            "temple": "TempleOS V5.03 Public Domain Operating System",
        }
        for machine, text in screens.items():
            self.assertIsNotNone(screen_marker(machine, text), machine)
            for other in SCREEN_MARKERS:
                if other != machine:
                    self.assertIsNone(screen_marker(other, text), (machine, other))


if __name__ == "__main__":
    unittest.main()
