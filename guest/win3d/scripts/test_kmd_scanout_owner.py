#!/usr/bin/env python3
"""Who may change the scanout in the kernel driver, compiled and run on the build host.

Card #755 (finding F7 of docs/reviews/win3d-driver.md). The driver source is
not in this repository, so the test takes the header
viogpu/common/viogpu_scanout_owner.h out of the patch that adds it and
compiles tests/kmd/scanout_owner_test.cpp against it. The same program built
with -DOLD_DRIVER runs the checks on what the driver did before the patch,
and must fail them.

Run from the repository root: python3 -m unittest discover -s scripts -p 'test_*.py'
"""
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

WIN3D = Path(__file__).resolve().parent.parent
PATCH = WIN3D / 'patches/kmd/kmd-scanout-only-for-its-owner.patch'
HEADER = 'viogpu/common/viogpu_scanout_owner.h'
SOURCE = WIN3D / 'tests/kmd/scanout_owner_test.cpp'


def header_from_patch(text, path):
    """The content of a file that a patch adds as a new file."""
    lines, inside, body = [], False, False
    for line in text.split('\n'):
        if line.startswith('diff --git '):
            inside, body = line.endswith(' b/' + path), False
        elif inside and line.startswith('@@ '):
            if not line.startswith('@@ -0,0 '):
                raise ValueError(f'{path} is not a new file in the patch')
            body = True
        elif inside and body and line.startswith('+'):
            lines.append(line[1:])
    if not lines:
        raise ValueError(f'the patch does not add {path}')
    return '\n'.join(lines) + '\n'


def compiler():
    for name in (os.environ.get('CXX'), 'c++', 'g++', 'clang++'):
        if name and shutil.which(name):
            return name
    return None


class ScanoutOwner(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cxx = compiler()
        if cls.cxx is None:
            raise unittest.SkipTest('no C++ compiler on this host')
        cls.tmp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.tmp.name)
        (cls.root / 'viogpu_scanout_owner.h').write_text(header_from_patch(PATCH.read_text(), HEADER))

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_checks(self, name, *flags):
        exe = self.root / name
        subprocess.run([self.cxx, '-std=c++17', '-Wall', '-Wextra', '-Werror', *flags,
                        '-I', str(self.root), '-o', str(exe), str(SOURCE)], check=True)
        return subprocess.run([str(exe)], capture_output=True, text=True)

    def test_the_patched_checks_pass_every_case(self):
        r = self.run_checks('patched')
        self.assertEqual(r.returncode, 0, r.stdout)
        self.assertNotIn('FAIL', r.stdout)

    def test_the_driver_before_the_patch_fails_the_f7_cases(self):
        r = self.run_checks('old', '-DOLD_DRIVER')
        failed = [line[5:] for line in r.stdout.splitlines() if line.startswith('FAIL')]
        self.assertEqual(r.returncode, len(failed), r.stdout)
        self.assertIn('F7: a flip command in the stream of a program is refused', failed, r.stdout)
        self.assertIn('F7: a program shows no allocation of its own while the desktop has the scanout', failed, r.stdout)
        self.assertEqual(len(failed), 10, r.stdout)
        # Nothing but the F7 cases fails: the user-mode driver worked before.
        self.assertTrue(all(f.startswith('F7: ') for f in failed), r.stdout)

    def test_the_driver_uses_both_checks(self):
        patch = PATCH.read_text()
        self.assertIn('+            if (!VioGpuUserStreamCommandAllowed(cmdHdr.type))', patch)
        self.assertIn('+                return STATUS_PRIVILEGED_INSTRUCTION;', patch)
        self.assertIn('+                const int refusal = VioGpuScanoutEscapeCheck(pVioGpuEscape->ResourceSetScanoutBlob.ScanoutId,', patch)
        self.assertIn('STATUS_ACCESS_DENIED;', patch)
        # Render keeps no second place that reads a flip command of a program.
        self.assertIn('-            if (cmdHdr.type == VIOGPU_CMD_PRESENT_FLIP)', patch)

    def test_the_flip_command_number_is_the_one_of_the_driver(self):
        # The header has no include, so it names the command by its number.
        # kmd-flip-trace-build.patch quotes the dispatch of the driver.
        header = (self.root / 'viogpu_scanout_owner.h').read_text()
        self.assertIn('return command_type != 4;', header)

    def test_the_header_has_no_include(self):
        self.assertNotIn('#include', (self.root / 'viogpu_scanout_owner.h').read_text())


if __name__ == '__main__':
    unittest.main()
