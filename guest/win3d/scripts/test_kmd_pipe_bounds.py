#!/usr/bin/env python3
"""The message read of the helpers' pipe, compiled and run on the build host.

Card #757 (finding F9 of docs/reviews/win3d-driver.md). The driver source is
not in this repository, so the test takes the header
viogpu/shared/pipe_bounds.h out of the patch that adds it and compiles
tests/kmd/pipe_bounds_test.cpp against it. The same program built with
-DOLD_DRIVER runs the checks on the two read loops of the helpers before the
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
PATCH = WIN3D / 'patches/kmd/kmd-read-helper-pipe-message-inside-buffer.patch'
HEADER = 'viogpu/shared/pipe_bounds.h'
SOURCE = WIN3D / 'tests/kmd/pipe_bounds_test.cpp'


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


class PipeBounds(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.patch = PATCH.read_text()
        cls.cxx = compiler()
        if cls.cxx is None:
            raise unittest.SkipTest('no C++ compiler on this host')
        cls.tmp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.tmp.name)
        (cls.root / 'pipe_bounds.h').write_text(header_from_patch(cls.patch, HEADER))

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_checks(self, name, *flags):
        exe = self.root / name
        subprocess.run([self.cxx, '-std=c++17', '-Wall', '-Wextra', '-Werror', *flags,
                        '-I', str(self.root), '-o', str(exe), str(SOURCE)], check=True)
        return subprocess.run([str(exe)], capture_output=True, text=True, timeout=300)

    def test_the_patched_read_passes_every_case(self):
        r = self.run_checks('patched')
        self.assertEqual(r.returncode, 0, r.stdout)
        self.assertNotIn('FAIL', r.stdout)

    def test_the_read_loops_before_the_patch_fail_the_f9_cases(self):
        r = self.run_checks('old', '-DOLD_DRIVER')
        failed = [line[5:] for line in r.stdout.splitlines() if line.startswith('FAIL')]
        self.assertEqual(r.returncode, len(failed), r.stdout)
        # The claim of the review: one message of 3000 bytes overflows the
        # buffer of the service, and a second piece that of the client.
        self.assertIn('F9: server: a message of 3000 bytes writes nothing past the buffer', failed, r.stdout)
        self.assertIn('F9: client: a message of 3000 bytes writes nothing past the buffer', failed, r.stdout)
        self.assertIn('F9: server: a message of 2048 bytes leaves a string that ends inside the buffer', failed, r.stdout)
        # Nothing but the F9 cases fails: a correct peer worked before.
        self.assertTrue(failed and all(f.startswith('F9: ') for f in failed), r.stdout)

    def test_both_helpers_read_through_the_header(self):
        added = [line[1:].strip() for line in self.patch.splitlines() if line.startswith('+')]
        removed = [line[1:].strip() for line in self.patch.splitlines() if line.startswith('-')]
        self.assertEqual(added.count('int result = VioGpuPipeReadMessage(m_buffer,'), 2)
        self.assertIn('#define DATA_BUFFER_SIZE   (DATA_BUFFER_LENGTH * sizeof(wchar_t))', added)
        self.assertIn('int left = DATA_BUFFER_SIZE * sizeof(wchar_t);', removed)
        self.assertIn('bFinishedRead = ::ReadFile(m_hPipe, &m_buffer[read], DATA_BUFFER_SIZE, &drBytes, NULL);', removed)
        # No ReadFile is left that takes its length from a macro.
        self.assertEqual([a for a in added if 'ReadFile(' in a and '::ReadFile(m_hPipe, destination, room, &drBytes, NULL);' not in a
                          and 'PrintMessage' not in a], [])

    def test_the_header_has_no_include(self):
        self.assertNotIn('#include', (self.root / 'pipe_bounds.h').read_text())


if __name__ == '__main__':
    unittest.main()
