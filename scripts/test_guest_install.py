"""Exercise the guest-install recipe with prebuilt fixture assets."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class GuestInstallTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.home = self.root / 'home'
        self.dest = self.home / '.local/share/vmlab/guest'
        self.dest.parent.mkdir(parents=True)
        dist = self.root / 'guest/dist'
        dist.mkdir(parents=True)
        (dist / 'current-asset').write_text('current')

    def install(self):
        return subprocess.run(
            ['just', '--justfile', str(ROOT / 'justfile'), '--working-directory',
             str(self.root), '--no-deps', 'guest-install'],
            env=dict(os.environ, HOME=str(self.home)), capture_output=True, text=True,
        )

    def test_replaces_old_guest_directory(self):
        stale = self.dest / 'agent/old-target/vmlab-agent'
        stale.parent.mkdir(parents=True)
        stale.write_text('old')

        result = self.install()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(stale.exists(), f'Stale guest asset survived: {stale}')
        self.assertEqual((self.dest / 'current-asset').read_text(), 'current')

    def test_refuses_symlinked_guest_directory(self):
        target = self.root / 'unrelated'
        target.mkdir()
        self.dest.symlink_to(target, target_is_directory=True)

        result = self.install()

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('symlink', result.stderr)
        self.assertTrue(self.dest.is_symlink())
        self.assertEqual(list(target.iterdir()), [])
