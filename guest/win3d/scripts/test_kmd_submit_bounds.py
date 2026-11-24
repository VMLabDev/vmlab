#!/usr/bin/env python3
"""The size check of the submit escape of the kernel driver, compiled and run on the build host.

Card #752 (finding F4 of docs/reviews/win3d-driver.md). The driver source is
not in this repository, so the test takes the header
viogpu/common/viogpu_submit_bounds.h out of the patch that adds it and
compiles tests/kmd/submit_bounds_test.cpp against it. The same program built
with -DOLD_DRIVER runs the checks on the expression of the driver before the
patch, and must fail them.

Run from the repository root: python3 -m unittest discover -s scripts -p 'test_*.py'
"""
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

WIN3D = Path(__file__).resolve().parent.parent
PATCH = WIN3D / 'patches/kmd/kmd-check-submit-escape-size.patch'
HEADER = 'viogpu/common/viogpu_submit_bounds.h'
SOURCE = WIN3D / 'tests/kmd/submit_bounds_test.cpp'


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


class SubmitBounds(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cxx = compiler()
        if cls.cxx is None:
            raise unittest.SkipTest('no C++ compiler on this host')
        cls.tmp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.tmp.name)
        (cls.root / 'viogpu_submit_bounds.h').write_text(header_from_patch(PATCH.read_text(), HEADER))

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_checks(self, name, *flags):
        exe = self.root / name
        subprocess.run([self.cxx, '-std=c++17', '-Wall', '-Wextra', '-Werror', *flags,
                        '-I', str(self.root), '-o', str(exe), str(SOURCE)], check=True)
        return subprocess.run([str(exe)], capture_output=True, text=True)

    def test_the_patched_check_passes_every_case(self):
        r = self.run_checks('patched')
        self.assertEqual(r.returncode, 0, r.stdout)
        self.assertNotIn('FAIL', r.stdout)

    def test_the_check_before_the_patch_fails_the_f4_cases(self):
        r = self.run_checks('old', '-DOLD_DRIVER')
        failed = [line[5:] for line in r.stdout.splitlines() if line.startswith('FAIL')]
        self.assertEqual(r.returncode, len(failed), r.stdout)
        self.assertIn('F4: CmdSize 0xFFFFFFF8 is refused with DataLength 12', failed, r.stdout)
        self.assertIn('F4: CmdSize 0xFFFFFFF4, the first that wraps, is refused', failed, r.stdout)
        # Nothing but the F4 cases fails: a correct user worked before.
        self.assertTrue(failed and all(f.startswith('F4: ') for f in failed), r.stdout)

    def test_the_driver_uses_the_check_and_no_sum(self):
        patch = PATCH.read_text()
        self.assertIn('+                if (!VioGpuSubmitCmdFits(pVioGpuEscape->DataLength, (UINT)sizeof(*req), cmd_size))', patch)
        self.assertIn('-                const UINT needed = (UINT)(sizeof(*req) + req->CmdSize);', patch)

    def test_the_header_has_no_include(self):
        self.assertNotIn('#include', (self.root / 'viogpu_submit_bounds.h').read_text())


if __name__ == '__main__':
    unittest.main()
