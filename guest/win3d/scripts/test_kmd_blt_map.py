#!/usr/bin/env python3
"""Where the kernel driver maps the surfaces of a Blt present copy, compiled and run on the build host.

Card #756 (finding F8 of docs/reviews/win3d-driver.md). The driver source is
not in this repository, so the test takes the header
viogpu/viogpu3d/viogpu_blt_map.h out of the patch that adds it and compiles
tests/kmd/blt_map_test.cpp against it. The same program built with
-DOLD_DRIVER runs the checks on the expressions of the driver before the
patch, and must fail them.

Run from the repository root: python3 -m unittest discover -s scripts -p 'test_*.py'
"""
import subprocess
import tempfile
import unittest
from pathlib import Path

from test_kmd_submit_bounds import compiler, header_from_patch

WIN3D = Path(__file__).resolve().parent.parent
PATCH = WIN3D / 'patches/kmd/kmd-map-present-source-in-present.patch'
HEADER = 'viogpu/viogpu3d/viogpu_blt_map.h'
SOURCE = WIN3D / 'tests/kmd/blt_map_test.cpp'


def removed_and_added(patch, path):
    """The removed and the added lines of one file of a patch, each as one text."""
    removed, added, inside = [], [], False
    for line in patch.split('\n'):
        if line.startswith('diff --git '):
            inside = line.endswith(' b/' + path)
        elif inside and line.startswith('-') and not line.startswith('---'):
            removed.append(line[1:].strip())
        elif inside and line.startswith('+') and not line.startswith('+++'):
            added.append(line[1:].strip())
    return ' '.join(removed), ' '.join(added)


class BltMap(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cxx = compiler()
        if cls.cxx is None:
            raise unittest.SkipTest('no C++ compiler on this host')
        cls.tmp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.tmp.name)
        (cls.root / 'viogpu_blt_map.h').write_text(header_from_patch(PATCH.read_text(), HEADER))

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_checks(self, name, *flags):
        exe = self.root / name
        subprocess.run([self.cxx, '-std=c++17', '-Wall', '-Wextra', '-Werror', *flags,
                        '-I', str(self.root), '-o', str(exe), str(SOURCE)], check=True)
        return subprocess.run([str(exe)], capture_output=True, text=True)

    def test_the_patched_rules_pass_every_case(self):
        r = self.run_checks('patched')
        self.assertEqual(r.returncode, 0, r.stdout)
        self.assertNotIn('FAIL', r.stdout)

    def test_the_rules_before_the_patch_fail_the_f8_cases(self):
        r = self.run_checks('old', '-DOLD_DRIVER')
        failed = [line[5:] for line in r.stdout.splitlines() if line.startswith('FAIL')]
        self.assertEqual(r.returncode, len(failed), r.stdout)
        self.assertIn('F8: a blob source without the mappable flag is refused for a GDI destination', failed, r.stdout)
        self.assertIn('F8: a source that is not resident is mapped in Present, not in Patch', failed, r.stdout)
        # Nothing but the F8 cases fails: a correct user worked before.
        self.assertTrue(failed and all(f.startswith('F8: ') for f in failed), r.stdout)

    def test_the_driver_uses_both_rules(self):
        patch = PATCH.read_text()
        _, command = removed_and_added(patch, 'viogpu/viogpu3d/viogpu_command.cpp')
        _, device = removed_and_added(patch, 'viogpu/viogpu3d/viogpu_device.cpp')
        self.assertIn('return VioGpuBltSurfacesSupported(src->IsBlob(), src->GetBlobFlags(), '
                      'dst->IsBlob(), dst->GetBlobFlags())', command)
        self.assertIn('const int map = VioGpuBltPresentMap( dst->IsBlob(), '
                      'dxgk_src->SegmentId != 0 && dxgk_dst->SegmentId != 0, '
                      'src->HasCpuCopyBacking() && dst->HasCpuCopyBacking());', device)
        self.assertIn('NTSTATUS mapStatus = map == VIOGPU_BLT_PRESENT_MAP_BOTH ? cmd->MapCpuCopyBlt(m_id) '
                      ': cmd->MapCpuCopyBltSource(m_id);', device)

    def test_patch_does_not_return_the_status_of_a_failed_mapping(self):
        removed, _ = removed_and_added(PATCH.read_text(), 'viogpu/viogpu3d/viogpu_command.cpp')
        # The one `return status;` the patch removes is the one of Patch.
        self.assertIn('// WDDM treats any Patch failure as fatal (bugcheck 0x119/3). '
                      '// Fail closed: this is not recoverable error reporting, and success '
                      '// would retire an invalid CPU BLT as a successful NOP. return status;', removed)

    def test_the_header_has_no_include(self):
        self.assertNotIn('#include', (self.root / 'viogpu_blt_map.h').read_text())


if __name__ == '__main__':
    unittest.main()
