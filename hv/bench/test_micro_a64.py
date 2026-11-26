import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import lib
import micro
import micro_a64
import run


class MicroA64Tests(unittest.TestCase):
    def test_backends_route_without_a_template(self):
        self.assertEqual(run.main(["--backend", "interpreter", "--arch", "aarch64",
                                   "--vcpus", "1", "--dry-run"]), 0)
        self.assertEqual(run.main(["--backend", "qemu", "--arch", "aarch64",
                                   "--guest", "flat-a64", "--vcpus", "1", "--dry-run"]), 0)

    def test_riscv_interpreter_keeps_its_route(self):
        with mock.patch.object(micro_a64, "run") as a64:
            self.assertEqual(run.main(["--backend", "interpreter", "--arch", "riscv64",
                                       "--vcpus", "1", "--dry-run"]), 0)
            a64.assert_not_called()

    def test_wrong_arch_and_cpu_count_are_refused(self):
        with self.assertRaises(lib.BenchError):
            run.main(["--backend", "qemu", "--arch", "riscv64", "--guest", "flat-a64",
                      "--dry-run"])
        with self.assertRaises(lib.BenchError):
            run.main(["--backend", "interpreter", "--arch", "aarch64", "--vcpus", "4",
                      "--dry-run"])

    def test_loop_count(self):
        self.assertEqual(micro_a64.loop_expected_instructions(1), 17)
        self.assertEqual(micro_a64.loop_expected_instructions(micro_a64.LOOP_ITERATIONS),
                         300_000_014)
        for iterations in (0, 1 << 32):
            with self.assertRaises(lib.BenchError):
                micro_a64.build_loop(iterations)

    def test_images_use_no_fp_and_only_aligned_accesses(self):
        flags = micro_a64.a64_flags()
        self.assertIn("-mgeneral-regs-only", flags)
        self.assertIn("-mstrict-align", flags)

    def test_coremark_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "target/tcg/coremark").mkdir(parents=True)
            with mock.patch.object(lib, "REPO_DIR", root), \
                    mock.patch.object(subprocess, "check_output",
                                      side_effect=[micro.COREMARK_REVISION + "\n", ""]), \
                    mock.patch.object(subprocess, "run") as execute:
                elf, image = micro_a64.build_coremark(1000)
            self.assertEqual((elf.name, image.name), ("coremark-1000.elf", "coremark-1000.bin"))
            compiles = [call.args[0] for call in execute.call_args_list
                        if call.args[0][0] == "clang"]
            self.assertEqual(len(compiles), 6)
            self.assertTrue(all("-DITERATIONS=1000" in command for command in compiles))
        with self.assertRaises(lib.BenchError):
            micro_a64.build_coremark(0)

    def test_the_guest_verdict(self):
        good = {"exit_code": 0, "x2": 0xE714, "x3": 0x1FD7, "x4": 0x8E3A, "x5": 0}
        micro_a64.check_guest("coremark_algorithms_1000", good)
        micro_a64.check_guest("scalar_loop_300000000", {"exit_code": 0, "x2": 0})
        with self.assertRaises(lib.BenchError):
            micro_a64.check_guest("coremark_algorithms_1000", {**good, "x3": 0})
        with self.assertRaises(lib.BenchError):
            micro_a64.check_guest("scalar_loop_300000000", {"exit_code": 1})

    def test_record_holds_seconds_and_mips(self):
        rec = micro_a64.record("interpreter", "w", "c", False, "h", [3.0, 1.0, 2.0],
                               [100.0, 300.0, 150.0], {}, [])
        self.assertEqual((rec["median"], rec["spread"], rec["mips_median"]), (2.0, 2.0, 150.0))
        self.assertEqual((rec["arch"], rec["guest"], rec["unit"], rec["repeats"]),
                         ("aarch64", "flat-a64", "s", 3))


if __name__ == "__main__":
    unittest.main()
