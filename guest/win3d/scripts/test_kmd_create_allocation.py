#!/usr/bin/env python3
"""The fix of DxgkCreateAllocation in the kernel driver, checked on the build host.

Card #757 (finding F10 of docs/reviews/win3d-driver.md). The driver source is
not in this repository and DxgkCreateAllocation does not compile outside the
WDK, so this test has two parts. It reads the patch for the three changes of
the fix. And it compiles tests/kmd/pool_new_test.cpp with the declaration of
the driver's operator new that the patch adds, and again with the one it
removes: with the old declaration the constructor runs on the NULL pointer,
which is why the NULL check alone would not be a fix.

The guest proof (a failed pool allocation in a running driver) is
tests/kmd/alloc-probe.sh.

Run from the repository root: python3 -m unittest discover -s scripts -p 'test_*.py'
"""
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

WIN3D = Path(__file__).resolve().parent.parent
PATCH = WIN3D / 'patches/kmd/kmd-check-create-allocation-memory.patch'
SOURCE = WIN3D / 'tests/kmd/pool_new_test.cpp'
OLD = 'operator new(size_t Size, POOL_TYPE PoolType = PagedPool);'
NEW = 'operator new(size_t Size, POOL_TYPE PoolType = PagedPool) noexcept;'


def hunks(text, path):
    """The removed and the added lines of one file of a patch, in order."""
    removed, added, inside = [], [], False
    for line in text.split('\n'):
        if line.startswith('diff --git '):
            inside = line.endswith(' b/' + path)
        elif inside and line.startswith('-') and not line.startswith('---'):
            removed.append(line[1:])
        elif inside and line.startswith('+') and not line.startswith('+++'):
            added.append(line[1:])
    return removed, added


def compiler():
    for name in (os.environ.get('CXX'), 'c++', 'g++', 'clang++'):
        if name and shutil.which(name):
            return name
    return None


class CreateAllocationPatch(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.patch = PATCH.read_text()

    def test_both_allocators_are_noexcept_in_declaration_and_definition(self):
        removed, added = hunks(self.patch, 'viogpu/common/baseobj.h')
        self.assertIn(OLD, removed)
        self.assertIn(NEW, added)
        self.assertIn('operator new[](size_t Size, POOL_TYPE PoolType = PagedPool) noexcept;', added)
        removed, added = hunks(self.patch, 'viogpu/common/baseobj.cpp')
        self.assertEqual([a.strip() for a in added],
                         ['void* __cdecl operator new(size_t Size, POOL_TYPE PoolType) noexcept',
                          'operator new[](size_t Size, POOL_TYPE PoolType) noexcept'])
        self.assertEqual(len(removed), 2)

    def test_create_allocation_checks_the_pointer_before_it_uses_it(self):
        removed, added = hunks(self.patch, 'viogpu/viogpu3d/viogpu_allocation.cpp')
        self.assertEqual(removed, [])
        body = '\n'.join(a.strip() for a in added)
        self.assertIn('if (!allocation)\n{\n'
                      'DbgPrint(TRACE_LEVEL_ERROR, ("<--- %s no memory for the allocation\\n", __FUNCTION__));\n'
                      'return STATUS_INSUFFICIENT_RESOURCES;\n}', body)
        # The check is the first line after the new expression.
        lines = self.patch.split('\n')
        at = lines.index('     VioGpuAllocation *allocation = new (NonPagedPoolNx) VioGpuAllocation(adapter, &localExchange);')
        self.assertEqual(lines[at + 1], '+    if (!allocation)')
        self.assertEqual(lines[at + 6], '     if (!allocation->IsValid())')

    def test_create_allocation_refuses_a_count_other_than_one(self):
        _, added = hunks(self.patch, 'viogpu/viogpu3d/viogpu_allocation.cpp')
        body = '\n'.join(a.strip() for a in added)
        self.assertIn('if (pCreateAllocation->NumAllocations != 1 || !pCreateAllocation->pAllocationInfo)', body)
        self.assertIn('return STATUS_INVALID_PARAMETER;', body)
        # Before the first read of pAllocationInfo.
        lines = self.patch.split('\n')
        check = lines.index('+    if (pCreateAllocation->NumAllocations != 1 || !pCreateAllocation->pAllocationInfo)')
        first_use = lines.index('     DXGK_ALLOCATIONINFO *allocationInfo = pCreateAllocation->pAllocationInfo;')
        self.assertLess(check, first_use)


class PoolNew(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cxx = compiler()
        if cls.cxx is None:
            raise unittest.SkipTest('no C++ compiler on this host')
        cls.tmp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.tmp.name)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_with(self, name, declaration, optimise):
        noexcept = 'noexcept' if declaration.endswith('noexcept;') else ''
        exe = self.root / name
        subprocess.run([self.cxx, '-std=c++17', optimise,
                        '-DPOOL_NEW_DECLARATION=' + declaration, '-DPOOL_NEW_NOEXCEPT=' + noexcept,
                        '-o', str(exe), str(SOURCE)], check=True)
        return subprocess.run([str(exe)], capture_output=True, text=True, timeout=60)

    def test_with_the_patched_declaration_no_constructor_runs_on_null(self):
        _, added = hunks(PATCH.read_text(), 'viogpu/common/baseobj.h')
        self.assertIn(NEW, added)
        for optimise in ('-O0', '-O2'):
            r = self.run_with('patched' + optimise, NEW, optimise)
            self.assertEqual((r.returncode, r.stdout.strip()), (0, 'null'), optimise)

    def test_with_the_declaration_before_the_patch_the_constructor_runs_on_null(self):
        removed, _ = hunks(PATCH.read_text(), 'viogpu/common/baseobj.h')
        self.assertIn(OLD, removed)
        for optimise in ('-O0', '-O2'):
            r = self.run_with('old' + optimise, OLD, optimise)
            # Exit 1 with the message, or a signal where the compiler dropped the test of `this`.
            self.assertTrue((r.returncode == 1 and r.stdout.strip() == 'constructed-at-null') or r.returncode < 0,
                            f'{optimise}: exit {r.returncode}, {r.stdout!r}')


if __name__ == '__main__':
    unittest.main()
