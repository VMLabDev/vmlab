"""Offline release dry-run. Cosign is a contract stub, not a crypto test."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('release_assets', ROOT / 'scripts/release-assets.py')
ASSETS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ASSETS)


class ReleaseDryRun(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.dist = self.root / 'dist'
        self.dist.mkdir()
        self.bin = self.root / 'tools'
        self.bin.mkdir()
        self.asset = 'vmlab-1.2.3-linux-x86_64'
        (self.dist / self.asset).write_text('#!/bin/sh\nprintf executed >> "$MARKER"\n')
        for name in ['vmlab-guest-1.2.3.tar.gz', 'vmlab-hv-1.2.3-linux-x86_64.tar.gz']:
            (self.dist / name).write_bytes(b'fixture')
        self.env = dict(os.environ, GITHUB_SHA='a' * 40, GITHUB_RUN_ID='123',
                        PATH=str(self.bin) + ':' + os.environ['PATH'],
                        FIXTURES=str(self.dist), MARKER=str(self.root / 'executed'),
                        VMLAB_RELEASE_BASE_URL='https://fixture.invalid',
                        VMLAB_INSTALL_DIR=str(self.root / 'installed'),
                        VMLAB_GUEST_DIR=str(self.root / 'guest'))
        with patch.dict(os.environ, self.env):
            ASSETS.prepare(self.dist, '1.2.3')
        (self.dist / 'SHA256SUMS.bundle').write_text('offline-signature-stub')
        self.tool('curl', '''#!/bin/sh
set -eu
name="${4##*/}"
[ "${FAIL_DOWNLOAD:-}" != "$name" ] || exit 22
cat "$FIXTURES/$name" > "$3"
''')
        self.tool('cosign', '''#!/bin/sh
set -eu
[ "$1" = verify-blob ]
[ "$2" = --bundle ]
[ "$4" = --certificate-identity ]
[ "$5" = https://github.com/VMLabDev/vmlab/.github/workflows/ci.yml@refs/heads/main ]
[ "$6" = --certificate-oidc-issuer ]
[ "$7" = https://token.actions.githubusercontent.com ]
[ "${BAD_SIGNATURE:-0}" = 0 ]
''')

    def tool(self, name, body):
        path = self.bin / name
        path.write_text(body)
        path.chmod(0o755)

    def install(self, *args):
        return subprocess.run(['sh', str(ROOT / 'install.sh'), '--version', '1.2.3',
                               '--no-fastpath', '--skip-checks', *args],
                              env=self.env, capture_output=True, text=True)

    def refused(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.root / 'executed').exists())
        self.assertFalse((self.root / 'installed/vmlab').exists())

    def test_valid_binary(self):
        result = self.install('--no-guest')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.root / 'executed').exists())

    def test_bad_signature_never_executes(self):
        self.env['BAD_SIGNATURE'] = '1'
        self.refused(self.install('--no-guest'))

    def test_missing_signature(self):
        self.env['FAIL_DOWNLOAD'] = 'SHA256SUMS.bundle'
        self.refused(self.install('--no-guest'))

    def test_missing_manifest(self):
        self.env['FAIL_DOWNLOAD'] = 'SHA256SUMS'
        self.refused(self.install('--no-guest'))

    def test_corrupt_binary(self):
        (self.dist / self.asset).write_text('tampered')
        self.refused(self.install('--no-guest'))

    def test_wrong_version_manifest(self):
        path = self.dist / 'SHA256SUMS'
        path.write_text(path.read_text().replace(self.asset, 'vmlab-9.9.9-linux-x86_64'))
        self.refused(self.install('--no-guest'))

    def test_duplicate_entry(self):
        path = self.dist / 'SHA256SUMS'
        path.write_text(path.read_text() + next(line for line in path.read_text().splitlines(True)
                                               if self.asset in line))
        self.refused(self.install('--no-guest'))

    def test_corrupt_guest_not_extracted(self):
        (self.dist / 'vmlab-guest-1.2.3.tar.gz').write_text('tampered')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('checksum mismatch', result.stderr)
        self.assertFalse((self.root / 'guest').exists())

    def test_provenance_and_checksums(self):
        data = json.loads((self.dist / 'provenance.json').read_text())
        self.assertEqual(len(data['subject']), 3)
        self.assertEqual(data['predicateType'], 'https://slsa.dev/provenance/v1')
        for line in (self.dist / 'SHA256SUMS').read_text().splitlines():
            digest, name = line.split()
            self.assertEqual(digest, hashlib.sha256((self.dist / name).read_bytes()).hexdigest())

    def test_missing_release_asset(self):
        (self.dist / self.asset).unlink()
        with self.assertRaises(ValueError):
            ASSETS.prepare(self.dist, '1.2.3')

    def test_unsafe_inputs(self):
        for directory, version in [('/', '1.2.3'), (self.dist, '../escape')]:
            with self.assertRaises(ValueError):
                ASSETS.prepare(directory, version)


if __name__ == '__main__':
    unittest.main()
