#!/usr/bin/env python3
"""The capset bounds of the kernel driver, compiled and run on the build host.

Card #751 (findings F1, F3 and F6 of docs/reviews/win3d-driver.md). The driver
source is not in this repository, so the test takes the header
viogpu/common/viogpu_capset_bounds.h out of the patch that adds it and
compiles tests/kmd/capset_bounds_test.cpp against it. The same program built
with -DOLD_DRIVER runs the checks on the expressions of the driver before the
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
PATCH = WIN3D / 'patches/kmd/kmd-check-capset-escape.patch'
HEADER = 'viogpu/common/viogpu_capset_bounds.h'
SOURCE = WIN3D / 'tests/kmd/capset_bounds_test.cpp'


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


class CapsetBounds(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cxx = compiler()
        if cls.cxx is None:
            raise unittest.SkipTest('no C++ compiler on this host')
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name)
        (root / 'viogpu_capset_bounds.h').write_text(header_from_patch(PATCH.read_text(), HEADER))
        cls.root = root

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_checks(self, name, *flags):
        exe = self.root / name
        subprocess.run([self.cxx, '-std=c++17', '-Wall', '-Wextra', '-Werror', *flags,
                        '-I', str(self.root), '-o', str(exe), str(SOURCE)], check=True)
        return subprocess.run([str(exe)], capture_output=True, text=True)

    def test_the_patched_bounds_pass_every_check(self):
        r = self.run_checks('patched')
        self.assertEqual(r.returncode, 0, r.stdout)
        self.assertNotIn('FAIL', r.stdout)

    def test_the_bounds_before_the_patch_fail_the_f3_and_f6_checks(self):
        r = self.run_checks('old', '-DOLD_DRIVER')
        failed = [line[5:] for line in r.stdout.splitlines() if line.startswith('FAIL')]
        self.assertEqual(r.returncode, len(failed), r.stdout)
        self.assertTrue(any(f.startswith('F3: id 0xFFFFFFC4') for f in failed), r.stdout)
        self.assertTrue(any(f.startswith('F6: the escape copies no byte') for f in failed), r.stdout)
        # Nothing but the F3 and F6 checks fails: a correct user and a correct host worked before.
        self.assertTrue(all(f.startswith(('F3: ', 'F6: ')) for f in failed), r.stdout)

    def test_the_header_has_no_include(self):
        self.assertNotIn('#include', (self.root / 'viogpu_capset_bounds.h').read_text())


if __name__ == '__main__':
    unittest.main()
