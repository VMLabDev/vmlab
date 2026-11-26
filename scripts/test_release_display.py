#!/usr/bin/env python3
"""Offline tests of scripts/release-display-from.sh: staging the signed Windows
display package of a published release (card 444). curl and cosign are stubs,
so this proves the order of the checks and the refusals, and no cryptography.
package.py is the real one, against a small fixture checkout."""

import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / 'scripts/release-display-from.sh'
PACKAGE_PY = ROOT / 'guest/vdisplay-windows/package.py'
MEMBERS = ['vmlab-display.inf', 'vmlab-display.cat', 'vmlab-display.dll', 'vmlab-display.cer',
           'install.exe', 'install.ps1', 'BUILDINFO.json', 'ivshmem/ivshmem.inf',
           'ivshmem/ivshmem.cat', 'ivshmem/ivshmem.sys']


def sha256(data):
    return hashlib.sha256(data).hexdigest()


class StageDisplayPackage(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        # A checkout with one build input, and the real package.py.
        self.checkout = self.root / 'checkout'
        source = self.checkout / 'guest/vdisplay-windows'
        source.mkdir(parents=True)
        shutil.copyfile(PACKAGE_PY, source / 'package.py')
        (source / 'driver.cpp').write_text('int driver;\n')
        (self.checkout / 'build').mkdir()
        (self.checkout / 'build/release-signing.pub').write_text('public-half\n')
        # The release 1.2.3: a guest bundle that holds a package signed for that input.
        self.release = self.root / 'release'
        self.release.mkdir()
        files = {name: name.encode() for name in MEMBERS if name != 'BUILDINFO.json'}
        files['BUILDINFO.json'] = json.dumps(
            {'sources': {'driver.cpp': sha256(b'int driver;\n')}}).encode()
        manifest = {'files': {name: sha256(data) for name, data in files.items()}}
        files['PACKAGE.json'] = json.dumps(manifest).encode()
        files['LICENSE-Microsoft.txt'] = b'ms'
        files['LICENSE-IVSHMEM.txt'] = b'ivshmem'
        bundle = self.release / 'vmlab-guest-1.2.3.tar.gz'
        with tarfile.open(bundle, 'w:gz') as archive:
            for name, data in files.items():
                info = tarfile.TarInfo(f'vdisplay-windows/x86_64/{name}')
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
        (self.release / 'SHA256SUMS').write_text(
            f'{sha256(bundle.read_bytes())}  vmlab-guest-1.2.3.tar.gz\n')
        (self.release / 'SHA256SUMS.bundle').write_text('offline-signature-stub')
        self.bin = self.root / 'tools'
        self.bin.mkdir()
        # curl -fsSL --proto =https --tlsv1.2 -o <file> <url>
        self.tool('curl', '''#!/bin/sh
set -eu
[ "$5" = -o ]
cat "$FIXTURES/${7##*/}" > "$6" 2>/dev/null || exit 22
''')
        self.tool('cosign', '''#!/bin/sh
set -eu
[ "$1" = verify-blob ]
printf '%s\\n' "$2" >> "$COSIGN_LOG"
[ "${BAD_SIGNATURE:-0}" = 0 ]
''')
        self.env = dict(os.environ, PATH=str(self.bin) + ':' + os.environ['PATH'],
                        FIXTURES=str(self.release), COSIGN_LOG=str(self.root / 'cosign.log'),
                        BASE_URL='https://fixture.invalid')
        # Without COSIGN the script runs cosign in the build container.
        self.env['COSIGN'] = str(self.bin / 'cosign')
        self.staged = source / 'target/signed-package'

    def tool(self, name, body):
        path = self.bin / name
        path.write_text(body)
        path.chmod(0o755)

    def stage(self, tag='v1.2.3', **env):
        return subprocess.run(['bash', str(SCRIPT), tag, str(self.checkout)],
                              env=dict(self.env, **env), capture_output=True, text=True)

    def refused(self, result, words):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(words, result.stderr)
        self.assertFalse(self.staged.exists())

    def test_stages_the_package_of_a_keyless_release(self):
        result = self.stage()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / 'cosign.log').read_text().split(), ['--bundle'])
        for name in MEMBERS + ['PACKAGE.json', 'LICENSE-Microsoft.txt', 'LICENSE-IVSHMEM.txt']:
            self.assertTrue((self.staged / name).is_file(), name)

    def test_a_key_signed_release_is_verified_with_the_committed_key(self):
        (self.release / 'SHA256SUMS.bundle').unlink()
        (self.release / 'SHA256SUMS.key.bundle').write_text('offline-key-signature-stub')
        result = self.stage('1.2.3')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / 'cosign.log').read_text().split(), ['--key'])
        self.assertTrue((self.staged / 'PACKAGE.json').is_file())

    def test_replaces_a_package_that_was_staged_before(self):
        self.staged.mkdir(parents=True)
        (self.staged / 'stale').write_text('old')
        self.assertEqual(self.stage().returncode, 0)
        self.assertFalse((self.staged / 'stale').exists())

    def test_an_unsigned_release_is_refused(self):
        (self.release / 'SHA256SUMS.bundle').unlink()
        self.refused(self.stage(), 'has no signature')

    def test_a_bad_signature_is_refused(self):
        self.refused(self.stage(BAD_SIGNATURE='1'), 'does not verify')

    def test_a_bundle_that_the_manifest_does_not_describe_is_refused(self):
        (self.release / 'SHA256SUMS').write_text(f'{"0" * 64}  vmlab-guest-1.2.3.tar.gz\n')
        self.refused(self.stage(), 'does not have the sha256')
        (self.release / 'SHA256SUMS').write_text(f'{"0" * 64}  other.tar.gz\n')
        self.refused(self.stage(), 'does not name')

    def test_a_changed_build_input_is_refused(self):
        (self.checkout / 'guest/vdisplay-windows/driver.cpp').write_text('int changed;\n')
        self.refused(self.stage(), 'sign again with --display rebuild')

    def test_a_changed_package_member_is_refused(self):
        bundle = self.release / 'vmlab-guest-1.2.3.tar.gz'
        work = self.root / 'repack'
        with tarfile.open(bundle) as archive:
            archive.extractall(work, filter='data')
        (work / 'vdisplay-windows/x86_64/vmlab-display.dll').write_text('tampered')
        with tarfile.open(bundle, 'w:gz') as archive:
            archive.add(work / 'vdisplay-windows', arcname='vdisplay-windows')
        (self.release / 'SHA256SUMS').write_text(
            f'{sha256(bundle.read_bytes())}  vmlab-guest-1.2.3.tar.gz\n')
        self.refused(self.stage(), 'does not fit the sources')

    def test_unsafe_inputs_are_refused(self):
        for tag in ['', 'latest', '../1.2.3', '1.2.3/x']:
            self.refused(self.stage(tag), 'invalid release tag')
        result = subprocess.run(['bash', str(SCRIPT), 'v1.2.3', str(self.root)],
                                env=self.env, capture_output=True, text=True)
        self.refused(result, 'no vmlab checkout')

    def test_missing_cosign_is_refused(self):
        self.refused(self.stage(COSIGN=str(self.root / 'no-cosign')), 'need cosign')


if __name__ == '__main__':
    unittest.main()
