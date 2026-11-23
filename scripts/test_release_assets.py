"""Offline release dry-run. Cosign is a contract stub, not a crypto test."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import shutil
import tempfile
import tarfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
INSTALL = ROOT / 'install.sh'
PUBLIC_KEY = ROOT / 'build/release-signing.pub'
COMMIT = 'a' * 40
# A throwaway public key: the tests stub cosign, so only its text matters.
KEY = '''-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEfixturefixturefixturefixturefixture
fixturefixturefixturefixturefixturefixturefixturefixture==
-----END PUBLIC KEY-----'''
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
        self.helper = 'vmlab-fastpath-1.2.3-linux-x86_64'
        (self.dist / self.helper).write_text('#!/bin/sh\nexit 2\n')
        for name in ['vmlab-guest-1.2.3.tar.gz', 'vmlab-hv-1.2.3-linux-x86_64.tar.gz']:
            (self.dist / name).write_bytes(b'fixture')
        self.env = dict(os.environ,
                        PATH=str(self.bin) + ':' + os.environ['PATH'],
                        FIXTURES=str(self.dist), MARKER=str(self.root / 'executed'),
                        VMLAB_RELEASE_BASE_URL='https://fixture.invalid',
                        VMLAB_INSTALL_DIR=str(self.root / 'installed'),
                        VMLAB_INSECURE_SKIP_SIGNATURE='0',
                        VMLAB_GUEST_DIR=str(self.root / 'guest'))
        self.prepare()
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
case "$2" in
  --bundle)
    [ "${3##*/}" = SHA256SUMS.bundle ]
    [ "$4" = --certificate-identity ]
    [ "$5" = https://github.com/VMLabDev/vmlab/.github/workflows/ci.yml@refs/heads/main ]
    [ "$6" = --certificate-oidc-issuer ]
    [ "$7" = https://token.actions.githubusercontent.com ]
    [ "${8##*/}" = SHA256SUMS ] ;;
  --key)
    [ "$(cat "$3")" = "$EXPECT_KEY" ]
    [ "$4" = --bundle ]
    [ "${5##*/}" = SHA256SUMS.key.bundle ]
    [ "$6" = --insecure-ignore-tlog=true ]
    [ "${7##*/}" = SHA256SUMS ] ;;
  *) exit 1 ;;
esac
printf '%s\\n' "$2" >> "$COSIGN_LOG"
[ "${BAD_SIGNATURE:-0}" = 0 ]
''')
        self.env['COSIGN_LOG'] = str(self.root / 'cosign.log')
        self.env['EXPECT_KEY'] = KEY

    def prepare(self):
        ASSETS.prepare(self.dist, '1.2.3', COMMIT, 'helios', image='sha256:' + 'b' * 64)

    def key_signed(self):
        """Turn the fixture into a release that scripts/release-sign.sh signed with a key."""
        (self.dist / 'SHA256SUMS.bundle').unlink()
        (self.dist / 'SHA256SUMS.key.bundle').write_text('offline-key-signature-stub')

    def verified_with(self):
        log = self.root / 'cosign.log'
        return log.read_text().split() if log.exists() else []

    def tool(self, name, body):
        path = self.bin / name
        path.write_text(body)
        path.chmod(0o755)

    def installer_with_key(self):
        """install.sh as it is after the operator commits the public half."""
        text = INSTALL.read_text()
        self.assertEqual(text.count("\nRELEASE_KEY=''\n"), 1)
        script = self.root / 'install-with-key.sh'
        script.write_text(text.replace("\nRELEASE_KEY=''\n", f"\nRELEASE_KEY='{KEY}'\n"))
        return script

    def install(self, *args, script=None):
        return subprocess.run(['sh', str(script or INSTALL), '--version', '1.2.3',
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

    def guest_bundle(self):
        agent = self.root / 'agent'
        agent.mkdir()
        (agent / 'new').write_text('new agent')
        with tarfile.open(self.dist / 'vmlab-guest-1.2.3.tar.gz', 'w:gz') as bundle:
            bundle.add(agent, arcname='agent')
        self.prepare()

    def test_guest_marker_allows_upgrade(self):
        self.guest_bundle()
        guest = self.root / 'guest'
        self.old_guest(guest)
        (guest / '.vmlab-guest').touch()
        (guest / 'old').write_text('obsolete')
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((guest / 'old').exists())
        self.assertTrue((guest / '.vmlab-guest').is_file())
        self.assertTrue((guest / 'agent/new').is_file())

    def old_guest(self, guest):
        (guest / 'agent/linux-x86_64').mkdir(parents=True)
        (guest / 'agent/linux-x86_64/vmlab-agent').write_text('old agent')
        (guest / 'firmware').mkdir()
        (guest / 'x86_64').mkdir()
        (guest / 'x86_64/vmlinuz').write_text('old kernel')
        (guest / 'x86_64/initramfs.img').write_text('old initramfs')

    def test_unmarked_old_guest_is_adopted_and_upgraded(self):
        self.guest_bundle()
        guest = self.root / 'guest'
        self.old_guest(guest)
        self.assertFalse((guest / '.vmlab-guest').exists())
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((guest / '.vmlab-guest').is_file())
        self.assertTrue((guest / 'agent/new').is_file())
        self.assertFalse((guest / 'agent/linux-x86_64/vmlab-agent').exists())

    def test_foreign_owned_guest_is_preserved(self):
        self.guest_bundle()
        guest = self.root / 'guest'
        self.old_guest(guest)
        # Simulate stat's ownership result without requiring root/chown.
        self.tool('stat', '#!/bin/sh\nset -eu\nprintf "%s\\n" foreign-uid\n')
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('non-owned', result.stderr)
        self.assertFalse((guest / '.vmlab-guest').exists())
        self.assertEqual((guest / 'x86_64/vmlinuz').read_text(), 'old kernel')

    def test_guest_without_marker_is_preserved(self):
        self.guest_bundle()
        guest = self.root / 'guest'
        guest.mkdir()
        (guest / 'valuable').write_text('keep')
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('refusing to replace unrelated', result.stderr)
        self.assertEqual((guest / 'valuable').read_text(), 'keep')

    def test_guest_symlink_is_preserved(self):
        self.guest_bundle()
        guest = self.root / 'guest'
        target = self.root / 'other'
        self.old_guest(target)
        (target / '.vmlab-guest').touch()
        guest.symlink_to(target)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(guest.is_symlink())

    def test_piped_help(self):
        result = subprocess.run(['sh', '-s', '--', '--help'],
                                input=(ROOT / 'install.sh').read_text(),
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Usage: install.sh', result.stdout)

    def test_latest_redirect_after_api_failure(self):
        self.tool('curl', '''#!/bin/sh
set -eu
case "$*" in
  *api.github.com*) exit 22 ;;
  *url_effective*) printf 'https://github.com/VMLabDev/vmlab/releases/tag/v1.2.3' ;;
  *) cat "$FIXTURES/${4##*/}" > "$3" ;;
esac
''')
        result = subprocess.run(['sh', str(ROOT / 'install.sh'), '--no-guest',
                                 '--no-fastpath', '--skip-checks'],
                                env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.root / 'installed/vmlab').exists())

    def test_noninteractive_default_never_grants_capabilities(self):
        self.tool('id', '#!/bin/sh\nset -eu\nprintf 1000\\n\n')
        self.tool('sudo', '#!/bin/sh\nset -eu\nexit 99\n')
        result = subprocess.run(['sh', str(ROOT / 'install.sh'), '--version',
                                 '1.2.3', '--no-guest', '--skip-checks'],
                                env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('no capabilities granted', result.stdout)

    def test_helper_is_installed_beside_the_binary(self):
        result = self.install('--no-guest')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        helper = self.root / 'installed/vmlab-fastpath'
        self.assertTrue(helper.is_file())
        self.assertTrue(os.access(helper, os.X_OK))

    def test_helper_is_executable_by_its_owner_only(self):
        # File capabilities go to whoever executes the file (review F2, card
        # 608): no other account may be able to start the helper.
        result = self.install('--no-guest')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        helper = self.root / 'installed/vmlab-fastpath'
        self.assertEqual(oct(helper.stat().st_mode & 0o777), oct(0o700))

    def test_helper_keeps_owner_only_mode_through_the_grant(self):
        self.fastpath_tools(swap_in=None)
        result = self.install('--no-guest', '--fastpath')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        helper = self.root / 'installed/vmlab-fastpath'
        self.assertEqual(oct(helper.stat().st_mode & 0o777), oct(0o700))
        self.assertIn('Any program running as you', result.stderr)
        self.assertIn('serves no other user', result.stderr)

    def fastpath_tools(self, *, swap_in):
        """Stubs for a grant. `swap_in` names the tool that, when the installer
        starts it, first renames another program onto the helper's path: what
        any program running as the user can do while the installer waits
        (review install-release F1, card 607). `setcap` records the digest of
        every file it is asked to grant."""
        self.env['CAP_LOG'] = str(self.root / 'cap.log')
        self.env['EVIL'] = str(self.root / 'evil')
        self.env['HELPER_PATH'] = str(self.root / 'installed/vmlab-fastpath')
        (self.root / 'evil').write_text('#!/bin/sh\necho not the helper\n')
        swap = ('cp "$EVIL" "$HELPER_PATH.x"\n'
                'chmod +x "$HELPER_PATH.x"\n'
                'mv "$HELPER_PATH.x" "$HELPER_PATH"\n')
        bodies = {
            'findmnt': 'printf "rw,relatime\\n"\n',
            'sudo': 'exec "$@"\n',
        }
        for name, body in bodies.items():
            self.tool(name, '#!/bin/sh\nset -eu\n' + (swap if name == swap_in else '') + body)
        self.tool('id', '#!/bin/sh\nset -eu\nprintf 1000\\n\n')
        self.tool('setcap', '#!/bin/sh\nset -eu\n[ "$1" = cap_bpf,cap_net_admin+ep ]\n'
                  'sha256sum "$2" | cut -d" " -f1 >> "$CAP_LOG"\n')
        self.tool('getcap', '#!/bin/sh\nprintf "%s cap_bpf,cap_net_admin=ep\\n" "$1"\n')

    def granted(self):
        """The digest of every file that was given the capabilities."""
        log = self.root / 'cap.log'
        return log.read_text().split() if log.exists() else []

    def helper_digest(self):
        return hashlib.sha256((self.dist / self.helper).read_bytes()).hexdigest()

    def staging_left(self):
        return [p.name for p in (self.root / 'installed').iterdir()
                if p.name not in ('vmlab', 'vmlab-fastpath')]

    def test_fastpath_grants_the_helper_and_never_the_binary(self):
        self.fastpath_tools(swap_in=None)
        result = self.install('--no-guest', '--fastpath')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        helper = self.root / 'installed/vmlab-fastpath'
        self.assertEqual(self.granted(), [self.helper_digest()])
        self.assertEqual(hashlib.sha256(helper.read_bytes()).hexdigest(), self.helper_digest())
        self.assertTrue(os.access(helper, os.X_OK))
        self.assertIn('Granted:', result.stdout)
        self.assertIn('only the vmlab-fastpath helper', result.stderr)
        self.assertEqual(self.staging_left(), [])

    def test_fastpath_refuses_a_file_swapped_in_before_the_grant(self):
        # The swap happens while the installer still prepares the grant, as it
        # would while the person reads the [y/N] prompt.
        self.fastpath_tools(swap_in='findmnt')
        result = self.install('--no-guest', '--fastpath')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.granted(), [])
        self.assertIn('is not the helper of this release', result.stderr)
        self.assertNotIn('Granted:', result.stdout)
        # No advice to grant the file that is at the path now.
        self.assertNotIn('sudo setcap', result.stdout + result.stderr)
        self.assertEqual(self.staging_left(), [])

    def test_fastpath_grants_only_verified_bytes_when_swapped_under_sudo(self):
        # The swap happens after sudo started, as it would while sudo waits
        # for its password.
        self.fastpath_tools(swap_in='sudo')
        result = self.install('--no-guest', '--fastpath')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.granted(), [self.helper_digest()])
        helper = self.root / 'installed/vmlab-fastpath'
        self.assertEqual(hashlib.sha256(helper.read_bytes()).hexdigest(), self.helper_digest())
        self.assertEqual(self.staging_left(), [])

    def test_release_without_helper_still_installs_the_binary(self):
        (self.dist / self.helper).unlink()
        self.tool('id', '#!/bin/sh\nset -eu\nprintf 1000\\n\n')
        self.tool('sudo', '#!/bin/sh\nset -eu\nexit 99\n')
        result = self.install('--no-guest', '--fastpath')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.root / 'installed/vmlab').exists())
        self.assertFalse((self.root / 'installed/vmlab-fastpath').exists())
        self.assertIn('fast path stays off', result.stderr)

    def test_corrupt_helper_is_refused(self):
        (self.dist / self.helper).write_text('tampered')
        result = self.install('--no-guest')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('checksum mismatch', result.stderr)
        self.assertFalse((self.root / 'installed/vmlab-fastpath').exists())

    def old_install(self):
        """The previous release in the bin directory: its vmlab and its helper,
        which holds the file capabilities of an earlier grant."""
        installed = self.root / 'installed'
        installed.mkdir()
        (installed / 'vmlab').write_text('#!/bin/sh\necho old vmlab\n')
        (installed / 'vmlab-fastpath').write_text('#!/bin/sh\necho old helper\n')
        for name in ('vmlab', 'vmlab-fastpath'):
            (installed / name).chmod(0o700)
        return installed

    def test_old_helper_is_removed_when_the_release_gives_none(self):
        # Review install-release F5, card 636: a helper download that fails
        # must not leave the previous, capable helper beside the new vmlab.
        installed = self.old_install()
        self.env['FAIL_DOWNLOAD'] = self.helper
        result = self.install('--no-guest')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn('old vmlab', (installed / 'vmlab').read_text())
        self.assertFalse((installed / 'vmlab-fastpath').exists())
        self.assertIn('fast path stays off', result.stderr)
        self.assertIn('Removed the vmlab-fastpath of the earlier install', result.stderr)

    def test_corrupt_helper_leaves_the_old_install_whole(self):
        # F5's second path: the helper is verified before vmlab is replaced,
        # so a refused helper never ends with a new vmlab beside the old one.
        installed = self.old_install()
        (self.dist / self.helper).write_text('tampered')
        result = self.install('--no-guest')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('checksum mismatch', result.stderr)
        self.assertIn('old vmlab', (installed / 'vmlab').read_text())
        self.assertIn('old helper', (installed / 'vmlab-fastpath').read_text())
        self.assertFalse((self.root / 'executed').exists())

    def test_bad_signature_never_executes(self):
        self.env['BAD_SIGNATURE'] = '1'
        self.refused(self.install('--no-guest'))

    # ── vmlab-hv, which the manual installs by hand (review F4) ──────────────

    HV = 'vmlab-hv-1.2.3-linux-x86_64'

    def hv_bundle(self, word):
        """A bundle whose icon installer writes `word` when it is run."""
        tree = self.root / 'hv-tree' / word / self.HV
        tree.mkdir(parents=True)
        script = tree / 'install-viewer-icons.sh'
        script.write_text(f'#!/bin/sh\nprintf {word} > "$MARKER.hv"\n')
        script.chmod(0o755)
        path = self.dist / f'{self.HV}.tar.gz'
        with tarfile.open(path, 'w:gz') as tar:
            tar.add(tree, arcname=self.HV)
        # The sidecar of release-publish.sh. No signature covers it, so
        # whoever replaces the bundle writes a matching one.
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        (self.dist / f'{path.name}.sha256').write_text(f'{digest}  {path.name}\n')

    def manual_hv_install(self):
        """Run the commands of the manual's section "Install vmlab-hv" as they are."""
        page = (ROOT / 'docs/manual/pages/getting-started/install.wcl').read_text()
        section = page.split('h3 "Install vmlab-hv"', 1)[1]
        commands = re.search(r"code sh \{ source = <<'SRC'\n(.*?)\nSRC\n", section, re.S)[1]
        commands, pinned = re.subn(r'(?m)^VERSION=.*$', 'VERSION=1.2.3', commands)
        self.assertEqual(pinned, 1)
        # curl as the manual and install.sh call it, served from the fixture
        # release. https://vmlab.io/install.sh is the installer of this tree.
        self.tool('curl', '''#!/bin/sh
set -eu
out=""; here=0; url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -fLO) here=1; shift ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
name="${url##*/}"
if [ "$name" = install.sh ]; then cat "$INSTALL_SH"; exit 0; fi
[ -f "$FIXTURES/$name" ] || exit 22
if [ -n "$out" ]; then cat "$FIXTURES/$name" > "$out"
elif [ "$here" = 1 ]; then cat "$FIXTURES/$name" > "./$name"
else cat "$FIXTURES/$name"
fi
''')
        home = self.root / 'home'
        work = self.root / 'downloads'
        home.mkdir()
        work.mkdir()
        script = self.root / 'manual-hv-install.sh'
        script.write_text(commands + '\n')
        env = dict(self.env, HOME=str(home), INSTALL_SH=str(INSTALL))
        result = subprocess.run(['sh', str(script)], cwd=work, env=env,
                                capture_output=True, text=True)
        return result, home / '.local/opt' / self.HV, self.root / 'executed.hv'

    def test_manual_hv_install_unpacks_a_genuine_bundle(self):
        self.hv_bundle('genuine')
        self.prepare()
        result, unpacked, marker = self.manual_hv_install()
        self.assertTrue(unpacked.is_dir(), result.stdout + result.stderr)
        self.assertEqual(marker.read_text(), 'genuine')
        self.assertEqual(self.verified_with(), ['--bundle'])
        self.assertFalse((self.root / 'installed/vmlab').exists())

    def test_manual_hv_install_refuses_a_replaced_bundle_with_its_sidecar(self):
        self.hv_bundle('genuine')
        self.prepare()
        # The asset host swaps the bundle and regenerates the unsigned sidecar.
        # It cannot make a new signed SHA256SUMS.
        self.hv_bundle('tampered')
        result, unpacked, marker = self.manual_hv_install()
        self.assertFalse(marker.exists(), 'the replaced bundle was unpacked and run:\n'
                         + result.stdout + result.stderr)
        self.assertFalse(unpacked.exists())
        self.assertIn('checksum mismatch', result.stderr)
        self.assertFalse((self.root / 'downloads' / f'{self.HV}.tar.gz').exists())

    def fetch(self, name, *args):
        work = self.root / 'fetched'
        work.mkdir(exist_ok=True)
        result = subprocess.run(['sh', str(INSTALL), '--version', '1.2.3', '--fetch', name, *args],
                                cwd=work, env=self.env, capture_output=True, text=True)
        return result, work

    def test_fetch_downloads_one_verified_asset_and_installs_nothing(self):
        result, work = self.fetch(f'{self.HV}.tar.gz')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((work / f'{self.HV}.tar.gz').read_bytes(), b'fixture')
        self.assertEqual(self.verified_with(), ['--bundle'])
        self.assertFalse((self.root / 'installed').exists())
        self.assertFalse((self.root / 'guest').exists())

    def test_fetch_refuses_without_a_good_signature(self):
        self.env['BAD_SIGNATURE'] = '1'
        result, work = self.fetch(f'{self.HV}.tar.gz')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(work.iterdir()), [])

    def test_fetch_refuses_what_the_signed_manifest_does_not_name(self):
        sidecar = f'{self.HV}.tar.gz.sha256'
        (self.dist / sidecar).write_text('0' * 64 + f'  {self.HV}.tar.gz\n')
        result, work = self.fetch(sidecar)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('missing or duplicate checksum', result.stderr)
        self.assertEqual(list(work.iterdir()), [])

    def test_fetch_takes_a_file_name_and_never_a_path(self):
        for name in ['../SHA256SUMS', '/tmp/x', 'a/b', '.hidden', '-x', '']:
            result, work = self.fetch(name)
            self.assertNotEqual(result.returncode, 0, name)
            self.assertEqual(list(work.iterdir()), [])
            self.assertEqual(self.verified_with(), [])

    def restricted_path(self, *, cosign=True, sha256sum=True):
        # Do not accidentally find a host-installed cosign or sha256sum.
        if not cosign:
            (self.bin / 'cosign').unlink()
        for name in ['sh', 'uname', 'cat', 'mktemp', 'awk', 'cut', 'chmod',
                     'mkdir', 'mv', 'rm', 'id', 'sed', 'head']:
            (self.bin / name).symlink_to(shutil.which(name))
        if sha256sum:
            (self.bin / 'sha256sum').symlink_to(shutil.which('sha256sum'))
        self.env['PATH'] = str(self.bin)

    def test_missing_cosign_refuses_by_default(self):
        self.restricted_path(cosign=False)
        result = self.install('--no-guest')
        self.refused(result)
        self.assertIn('need cosign', result.stderr)

    def test_insecure_flag_without_cosign(self):
        self.restricted_path(cosign=False)
        self.env['FAIL_DOWNLOAD'] = 'SHA256SUMS.bundle'
        result = self.install('--no-guest', '--insecure-skip-signature')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('An attacker can replace', result.stderr)
        self.assertTrue((self.root / 'executed').exists())

    def test_insecure_environment_without_cosign(self):
        self.restricted_path(cosign=False)
        self.env['VMLAB_INSECURE_SKIP_SIGNATURE'] = '1'
        result = self.install('--no-guest')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('INSECURE', result.stderr)

    def test_insecure_still_rejects_corrupt_binary(self):
        self.restricted_path(cosign=False)
        (self.dist / self.asset).write_text('tampered')
        self.refused(self.install('--no-guest', '--insecure-skip-signature'))

    def test_insecure_still_requires_manifest(self):
        self.restricted_path(cosign=False)
        self.env['FAIL_DOWNLOAD'] = 'SHA256SUMS'
        self.refused(self.install('--no-guest', '--insecure-skip-signature'))

    def test_missing_hash_tool_refuses(self):
        self.restricted_path(sha256sum=False)
        result = self.install('--no-guest')
        self.refused(result)
        self.assertIn('need sha256sum', result.stderr)

    def test_invalid_insecure_environment_refuses(self):
        self.env['VMLAB_INSECURE_SKIP_SIGNATURE'] = 'yes'
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
        self.assertEqual(len(data['subject']), 4)
        self.assertEqual(data['predicateType'], 'https://slsa.dev/provenance/v1')
        # Built on a named host from a named commit, not by a GitHub workflow.
        builder = data['predicate']['runDetails']['builder']['id']
        self.assertIn('helios', builder)
        self.assertIn(COMMIT, builder)
        self.assertNotIn('.github/workflows', json.dumps(data))
        definition = data['predicate']['buildDefinition']
        self.assertEqual(definition['resolvedDependencies'][0]['digest'], {'gitCommit': COMMIT})
        self.assertEqual(definition['internalParameters'], {'buildImage': 'sha256:' + 'b' * 64})
        self.assertEqual(ASSETS.verify(self.dist),
                         {'version': '1.2.3', 'commit': COMMIT, 'builder': builder,
                          'prerelease': 'false'})
        for line in (self.dist / 'SHA256SUMS').read_text().splitlines():
            digest, name = line.split()
            self.assertEqual(digest, hashlib.sha256((self.dist / name).read_bytes()).hexdigest())

    def test_missing_release_asset(self):
        for name in [self.asset, self.helper]:
            (self.dist / name).rename(self.root / name)
            with self.assertRaises(ValueError):
                ASSETS.prepare(self.dist, '1.2.3', COMMIT, 'helios')
            with self.assertRaises(ValueError):
                ASSETS.verify(self.dist)
            (self.root / name).rename(self.dist / name)
            ASSETS.verify(self.dist)

    def test_unsafe_inputs(self):
        for directory, version, commit, builder in [
                ('/', '1.2.3', COMMIT, 'helios'), (self.dist, '../escape', COMMIT, 'helios'),
                (self.dist, '1.2.3', 'abc123', 'helios'), (self.dist, '1.2.3', COMMIT, 'bad host')]:
            with self.assertRaises(ValueError):
                ASSETS.prepare(directory, version, commit, builder)

    def test_check_refuses_what_the_manifest_does_not_describe(self):
        ASSETS.verify(self.dist)
        guest = self.dist / 'vmlab-guest-1.2.3.tar.gz'
        guest.write_bytes(b'replaced after the manifest')
        with self.assertRaisesRegex(ValueError, 'provenance.json does not describe'):
            ASSETS.verify(self.dist)
        self.prepare()
        sums = self.dist / 'SHA256SUMS'
        sums.write_text(sums.read_text() + '0' * 64 + '  extra-file\n')
        with self.assertRaisesRegex(ValueError, 'SHA256SUMS does not match'):
            ASSETS.verify(self.dist)
        self.prepare()
        guest.write_bytes(b'')
        with self.assertRaisesRegex(ValueError, 'empty release asset'):
            ASSETS.verify(self.dist)

    # ── The two ways of a locally built release (scripts/release-sign.sh) ────

    def test_key_signed_release_is_refused_while_the_switch_is_off(self):
        self.assertIn("\nRELEASE_KEY=''\n", INSTALL.read_text())
        self.key_signed()
        result = self.install('--no-guest')
        self.refused(result)
        self.assertIn('has none to verify it with', result.stderr)
        self.assertEqual(self.verified_with(), [])

    def test_key_signed_release_installs_with_the_committed_key(self):
        self.key_signed()
        result = self.install('--no-guest', script=self.installer_with_key())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.verified_with(), ['--key'])
        self.assertTrue((self.root / 'executed').exists())

    def test_bad_key_signature_never_executes(self):
        self.key_signed()
        self.env['BAD_SIGNATURE'] = '1'
        self.refused(self.install('--no-guest', script=self.installer_with_key()))

    def test_another_key_than_the_committed_one_is_refused(self):
        self.key_signed()
        self.env['EXPECT_KEY'] = KEY.replace('MFkw', 'XXXX')
        self.refused(self.install('--no-guest', script=self.installer_with_key()))

    def test_keyless_releases_still_install_after_the_switch(self):
        result = self.install('--no-guest', script=self.installer_with_key())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.verified_with(), ['--bundle'])

    def test_release_key_option_verifies_a_test_build(self):
        self.key_signed()
        key = self.root / 'test.pub'
        key.write_text(KEY + '\n')
        for args in (['--release-key', str(key)], [f'--release-key={key}']):
            result = self.install('--no-guest', *args)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn('not the one of this installer', result.stderr)
        self.env['VMLAB_RELEASE_KEY'] = str(key)
        self.assertEqual(self.install('--no-guest').returncode, 0)
        self.assertEqual(self.verified_with(), ['--key'] * 3)

    def test_release_key_option_never_falls_back_to_keyless(self):
        key = self.root / 'test.pub'
        key.write_text(KEY + '\n')
        result = self.install('--no-guest', '--release-key', str(key))
        self.refused(result)
        self.assertIn('no key signature', result.stderr)
        self.refused(self.install('--no-guest', '--release-key', str(self.root / 'absent.pub')))

    def test_unsigned_release_needs_the_insecure_flag(self):
        (self.dist / 'SHA256SUMS.bundle').unlink()
        result = self.install('--no-guest')
        self.refused(result)
        self.assertIn('--insecure-skip-signature', result.stderr)
        result = self.install('--no-guest', '--insecure-skip-signature')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.verified_with(), [])

    def test_installer_key_is_the_committed_public_half(self):
        embedded = re.search(r"\nRELEASE_KEY='([^']*)'\n", INSTALL.read_text())[1]
        if PUBLIC_KEY.exists():
            self.assertEqual(embedded, PUBLIC_KEY.read_text().strip())
            self.assertIn('BEGIN PUBLIC KEY', embedded)
        else:
            self.assertEqual(embedded, '')

    def sign(self, *args, **env):
        return subprocess.run(['bash', str(ROOT / 'scripts/release-sign.sh'), str(self.dist), *args],
                              env=dict(self.env, **env), capture_output=True, text=True)

    def signer(self):
        """A cosign that signs and verifies by contract: the arguments release-sign.sh must give."""
        self.tool('cosign', '''#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "$COSIGN_LOG"
case "$1" in
  sign-blob)
    [ "$2 $3 $4 $5" = "--yes --key env://VMLAB_RELEASE_COSIGN_KEY --tlog-upload=false" ]
    [ "$6" = --bundle ]
    [ -n "$VMLAB_RELEASE_COSIGN_KEY" ]
    [ "${COSIGN_PASSWORD+set}" = set ]
    printf 'signed %s\\n' "$(sha256sum < "$8")" > "$7" ;;
  verify-blob)
    [ "$2" = --key ]
    [ "$4" = --bundle ]
    [ "$6" = --insecure-ignore-tlog=true ]
    [ "$(cat "$3")" = "$EXPECT_KEY" ]
    [ "$(cat "$5")" = "signed $(sha256sum < "$7")" ] ;;
  *) exit 1 ;;
esac
''')
        key = self.root / 'release.pub'
        key.write_text(KEY + '\n')
        return {'VMLAB_RELEASE_PUBLIC_KEY': str(key), 'VMLAB_RELEASE_COSIGN_KEY': 'private half'}

    def test_sign_none_leaves_the_manifest_alone(self):
        (self.dist / 'SHA256SUMS.key.bundle').write_text('stale')
        for args, env in ([['none'], {}], [[], {}], [[], {'VMLAB_RELEASE_SIGNING': 'none'}]):
            result = self.sign(*args, **env)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('UNSIGNED', result.stderr)
            self.assertFalse((self.dist / 'SHA256SUMS.key.bundle').exists())
            self.assertFalse((self.dist / 'SHA256SUMS.bundle').exists())

    def test_sign_with_a_key_signs_then_verifies(self):
        env = self.signer()
        for args, extra in ([['key'], {}], [[], {'VMLAB_RELEASE_SIGNING': 'key'}]):
            result = self.sign(*args, **env, **extra)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((self.dist / 'SHA256SUMS.key.bundle').is_file())
            self.assertFalse((self.dist / 'SHA256SUMS.bundle').exists())
            self.assertNotIn('private half', result.stdout + result.stderr)
        commands = [line.split()[0] for line in (self.root / 'cosign.log').read_text().splitlines()]
        self.assertEqual(commands, ['sign-blob', 'verify-blob'] * 2)
        # What it wrote is what install.sh takes.
        result = self.install('--no-guest', script=self.installer_with_key())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_sign_with_a_key_refuses_without_its_halves(self):
        env = self.signer()
        for missing in env:
            result = self.sign('key', **dict(env, **{missing: ''}))
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((self.dist / 'SHA256SUMS.key.bundle').exists())
        # No public half is committed until the operator turns the way on.
        if not PUBLIC_KEY.exists():
            result = self.sign('key', VMLAB_RELEASE_COSIGN_KEY='private half')
            self.assertIn('no public half', result.stderr)

    def test_sign_removes_a_signature_that_the_public_half_rejects(self):
        env = self.signer()
        result = self.sign('key', **dict(env, EXPECT_KEY='another public half'))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('halves differ', result.stderr)
        self.assertFalse((self.dist / 'SHA256SUMS.key.bundle').exists())

    def test_sign_refuses_an_unknown_way_and_a_missing_manifest(self):
        self.assertNotEqual(self.sign('keyless').returncode, 0)
        (self.dist / 'SHA256SUMS').unlink()
        self.assertNotEqual(self.sign('none').returncode, 0)


if __name__ == '__main__':
    unittest.main()
