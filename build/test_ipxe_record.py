"""The recorded iPXE hashes are compared with every build of their profile
(docs/reviews/firmware.md, F9). No test here builds iPXE."""

import hashlib
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
ipxe = runpy.run_path(str(ROOT / "guest/ipxe/build.py"))

TOOLS = ["gcc (GCC) 1.0", "GNU ld 1.0"]
OTHER_TOOLS = ["gcc (GCC) 2.0", "GNU ld 1.0"]
BUILT = {"a.rom": "1" * 64, "ipxe-source.tar": "2" * 64}
RECORD = f"# {TOOLS[0]}\n# {TOOLS[1]}\n{'1' * 64}  a.rom\n{'2' * 64}  ipxe-source.tar\n"


class IpxeRecordTest(unittest.TestCase):
    def test_the_hv_profile_is_compared_with_the_record_in_the_repository(self):
        plan = ipxe["PROFILES"]["hv"]
        self.assertEqual(plan["record"], ROOT / "guest/ipxe/SHA256SUMS.hv")
        tools, sums = ipxe["parse_record"](plan["record"].read_text())
        # The record names every output of the profile, and what compiled it.
        self.assertEqual(sorted(sums), sorted(plan["order"]))
        self.assertEqual(len(tools), 2)

    def test_a_build_equal_to_the_record_has_no_fault(self):
        self.assertEqual(ipxe["verify"](RECORD, BUILT, TOOLS), ([], []))

    def test_changed_bytes_are_a_fault_that_names_the_file(self):
        faults, _ = ipxe["verify"](RECORD, dict(BUILT, **{"a.rom": "3" * 64}), TOOLS)
        self.assertEqual(faults, [f"a.rom: recorded {'1' * 64}, built {'3' * 64}"])

    def test_an_output_on_one_side_only_is_a_fault(self):
        faults, _ = ipxe["verify"](RECORD, {"a.rom": "1" * 64, "b.rom": "4" * 64}, TOOLS)
        self.assertEqual(faults, ["b.rom: built, not recorded",
                                  "ipxe-source.tar: recorded, not built"])

    def test_another_toolchain_skips_the_roms_by_name_and_still_compares_the_source(self):
        changed = {"a.rom": "3" * 64, "ipxe-source.tar": "2" * 64}
        faults, notes = ipxe["verify"](RECORD, changed, OTHER_TOOLS)
        self.assertEqual(faults, [])
        self.assertEqual(len(notes), 1)
        self.assertIn("a.rom", notes[0])
        self.assertIn(OTHER_TOOLS[0], notes[0])
        self.assertIn(TOOLS[0], notes[0])
        faults, _ = ipxe["verify"](RECORD, dict(changed, **{"ipxe-source.tar": "5" * 64}), OTHER_TOOLS)
        self.assertEqual(faults, [f"ipxe-source.tar: recorded {'2' * 64}, built {'5' * 64}"])

    def finish(self, content, record_text=None, record=False):
        """Run the end of a build over one output file, against a record
        that holds the hash of b"rom" unless `record_text` is given."""
        directory = tempfile.TemporaryDirectory(prefix="vmlab-ipxe-record-")
        self.addCleanup(directory.cleanup)
        output = Path(directory.name)
        (output / "a.rom").write_bytes(content)
        path = output / "RECORD"
        if record_text is None:
            record_text = f"# {TOOLS[0]}\n# {TOOLS[1]}\n{hashlib.sha256(b'rom').hexdigest()}  a.rom\n"
        path.write_text(record_text)
        plan = {"sums": "SHA256SUMS", "order": ("a.rom",), "record": path}
        ipxe["finish"](plan, output, ["a.rom"], TOOLS, record=record)
        return output, path

    def test_the_end_of_a_build_passes_on_the_recorded_bytes(self):
        output, _ = self.finish(b"rom")
        self.assertEqual((output / "SHA256SUMS").read_text(),
                         f"{hashlib.sha256(b'rom').hexdigest()}  a.rom\n")

    def test_the_end_of_a_build_fails_on_other_bytes(self):
        with self.assertRaises(SystemExit) as failure:
            self.finish(b"other")
        self.assertIn("a.rom: recorded", str(failure.exception))
        self.assertIn("--record", str(failure.exception))

    def test_record_writes_the_record_with_the_toolchain(self):
        _, path = self.finish(b"other", record=True)
        self.assertEqual(ipxe["parse_record"](path.read_text()),
                         (TOOLS, {"a.rom": hashlib.sha256(b"other").hexdigest()}))
        # What it wrote is what the next build is compared with.
        self.finish(b"other", record_text=path.read_text())

    def test_the_rom_bytes_do_not_depend_on_the_checkout_path(self):
        # iPXE's build id is a checksum of objects whose debug information
        # names the build directory, unless the compiler maps it away.
        self.assertIn("EXTRA_CFLAGS=-ffile-prefix-map=/some/where/src=.",
                      ipxe["make_variables"]("/some/where/src"))
        self.assertNotIn("/some/where", ipxe["object_inputs"](TOOLS))

    def test_cached_objects_are_tied_to_the_flags_and_the_toolchain(self):
        self.assertNotEqual(ipxe["object_inputs"](TOOLS), ipxe["object_inputs"](OTHER_TOOLS))

    def checkout(self):
        """A git checkout shaped like upstream's: a tracked file in the
        legacy object directory, which ignores every object beside it."""
        directory = tempfile.TemporaryDirectory(prefix="vmlab-ipxe-objects-")
        self.addCleanup(directory.cleanup)
        source = Path(directory.name) / "upstream"
        (source / "src/bin").mkdir(parents=True)
        (source / "src/bin/.gitignore").write_text("*\n!.gitignore\n")
        (source / "src/main.c").write_text("int main;\n")
        git = ["git", "-C", str(source), "-c", "user.name=t", "-c", "user.email=t@example.invalid"]
        for arguments in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "pin"]):
            subprocess.run([*git, *arguments], check=True)
        return source

    def changes(self, source):
        return subprocess.check_output(
            ["git", "-C", str(source), "status", "--porcelain"], text=True)

    def test_removing_stale_objects_leaves_the_checkout_equal_to_the_pin(self):
        # The next build refuses a checkout with tracked changes.
        source = self.checkout()
        (source / "src/bin/old.o").write_text("object")
        (source / "src/bin-x86_64-efi").mkdir()
        (source / "src/bin-x86_64-efi/old.o").write_text("object")
        ipxe["clear_stale_objects"](source, TOOLS)
        self.assertFalse((source / "src/bin/old.o").exists())
        self.assertFalse((source / "src/bin-x86_64-efi").exists())
        self.assertEqual(self.changes(source), "")
        self.assertEqual((source.parent / "object-inputs").read_text(),
                         ipxe["object_inputs"](TOOLS))
        # The same inputs keep the objects. Other inputs remove them again.
        (source / "src/bin/new.o").write_text("object")
        ipxe["clear_stale_objects"](source, TOOLS)
        self.assertTrue((source / "src/bin/new.o").exists())
        ipxe["clear_stale_objects"](source, OTHER_TOOLS)
        self.assertFalse((source / "src/bin/new.o").exists())
        self.assertEqual(self.changes(source), "")

    def test_a_checkout_that_lost_its_object_directory_files_is_restored(self):
        source = self.checkout()
        (source / "src/bin/.gitignore").unlink()
        ipxe["restore_objects"](source)
        self.assertEqual(self.changes(source), "")

    def test_a_changed_source_file_is_not_restored(self):
        source = self.checkout()
        (source / "src/main.c").write_text("int other;\n")
        (source / "src/bin/.gitignore").write_text("changed\n")
        ipxe["restore_objects"](source)
        self.assertEqual(self.changes(source), " M src/bin/.gitignore\n M src/main.c\n")


if __name__ == "__main__":
    unittest.main()
