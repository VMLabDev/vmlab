"""The release key (card 1021): cosign in the build container, a key that is
only ever in the environment, and the recipes keygen, trust and doctor.

docker and cosign are stubs, so this proves the contract of the scripts (what
reaches an argument list, a file and a message) and no cryptography. The proof
with a real cosign and a throwaway key is on the card."""
import json
import os
from pathlib import Path
import re
import stat
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[1]
SWITCH = re.compile(r"^RELEASE_KEY='[^']*'$", re.M)
COMMIT = 'e' * 40
SECRET_KEY = 'fixture-private-key-NEVER-SHOWN'
SECRET_PASSWORD = 'fixture-password-NEVER-SHOWN'
PUBLIC = '''-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEfixturefixturefixturefixture
ixturefixturefixturefixturefixturefixturefixturefixturefixtu==
-----END PUBLIC KEY-----
'''
OTHER_PUBLIC = PUBLIC.replace('MFkw', 'XXXX')
PRIVATE = '''-----BEGIN ENCRYPTED SIGSTORE PRIVATE KEY-----
Zml4dHVyZQ==
-----END ENCRYPTED SIGSTORE PRIVATE KEY-----
'''

# A docker that has one image and runs "cosign" by contract. Every call is
# logged with its arguments and with what it got of the two variables, so a
# test can tell an argument from the environment. A bundle names the public
# half that verifies it and the manifest it signs.
STUB_DOCKER = r'''#!/usr/bin/env python3
import hashlib, json, os, sys
from pathlib import Path

args = sys.argv[1:]
state = Path(os.environ['STUB_STATE'])
with open(state / 'docker.log', 'a') as log:
    log.write(json.dumps({'args': args, 'env': {name: os.environ.get(name) for name in
                          ('VMLAB_RELEASE_COSIGN_KEY', 'COSIGN_PASSWORD')}}) + '\n')
label = os.environ.get('STUB_COSIGN_LABEL', 'v2.6.1')
if args[:1] in (['info'], ['version']):
    print('/var/lib/docker' if 'DockerRootDir' in ' '.join(args) else '27.0.0')
elif args[:2] == ['image', 'inspect']:
    if os.environ.get('STUB_NO_IMAGE'):
        sys.exit(1)
    if '-f' in args:
        print(label if 'Labels' in args[args.index('-f') + 1] else 'sha256:' + '0' * 64)
elif args[:1] == ['run']:
    cosign = args[args.index('cosign') + 1:]
    # Only a variable that `-e NAME` names reaches the container.
    passed = {args[i + 1] for i, word in enumerate(args) if word == '-e'}
    if any('=' in name for name in passed):
        sys.exit('stub docker: a value in an -e argument')
    digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
    if cosign[0] == 'sign-blob':
        assert cosign[1:5] == ['--yes', '--key', 'env://VMLAB_RELEASE_COSIGN_KEY', '--tlog-upload=false']
        assert {'VMLAB_RELEASE_COSIGN_KEY', 'COSIGN_PASSWORD'} <= passed
        key = os.environ['VMLAB_RELEASE_COSIGN_KEY']
        Path(cosign[6]).write_text(f"{os.environ['STUB_PAIR']} {digest(cosign[7])}\n")
    elif cosign[0] == 'verify-blob':
        assert not passed, 'verifying takes no secret'
        public = Path(cosign[2]).read_text()
        want = f"{hashlib.sha256(public.encode()).hexdigest()} {digest(cosign[6])}\n"
        sys.exit(0 if Path(cosign[4]).read_text() == want else 1)
    else:
        sys.exit('stub docker: unexpected cosign ' + ' '.join(cosign))
else:
    sys.exit('stub docker: unexpected ' + ' '.join(args))
'''


def pair_of(public):
    """What the stub writes into a bundle for the key pair of this public half."""
    import hashlib
    return hashlib.sha256(public.encode()).hexdigest()


class Machine:
    """A build machine: a copy of the release scripts, a stub docker, no cosign."""

    def __init__(self, tmp_path, committed=PUBLIC):
        self.tmp = tmp_path
        self.checkout = tmp_path / 'checkout'
        for name in ['scripts/release-sign.sh', 'scripts/release-verify.sh', 'scripts/release-publish.sh',
                     'scripts/release-cosign.sh', 'scripts/release-assets.py', 'scripts/release-keygen.sh',
                     'scripts/release-trust.sh', 'scripts/release-doctor.sh', 'scripts/install-cosign.sh',
                     'install.sh']:
            (self.checkout / name).parent.mkdir(parents=True, exist_ok=True)
            (self.checkout / name).write_bytes((ROOT / name).read_bytes())
        # The checkout as it is before `release::trust`: the switch is off.
        install = self.checkout / 'install.sh'
        text, count = SWITCH.subn("RELEASE_KEY=''", install.read_text())
        assert count == 1
        install.write_text(text)
        self.before_trust = text
        (self.checkout / 'build').mkdir()
        if committed is not None:
            (self.checkout / 'build/release-signing.pub').write_text(committed)
        self.tools = tmp_path / 'tools'
        self.tools.mkdir()
        (self.tools / 'docker').write_text(STUB_DOCKER)
        (self.tools / 'docker').chmod(0o755)
        self.state = tmp_path / 'state'
        self.state.mkdir()
        self.env = {k: v for k, v in os.environ.items()
                    if k not in ('COSIGN', 'COSIGN_PASSWORD') and not k.startswith('VMLAB_')}
        self.env.update(PATH=f"{self.tools}:{os.environ['PATH']}", STUB_STATE=str(self.state),
                        STUB_PAIR=pair_of(committed or PUBLIC))
        self.secrets = {'VMLAB_RELEASE_COSIGN_KEY': SECRET_KEY, 'COSIGN_PASSWORD': SECRET_PASSWORD}

    def script(self, name, *args, shell=('bash',), **env):
        return subprocess.run([*shell, str(self.checkout / 'scripts' / name), *map(str, args)],
                              capture_output=True, text=True, env=dict(self.env, **env))

    def release(self, version='1.2.3'):
        dist = self.tmp / f'release-{version}'
        dist.mkdir()
        for name in [f'vmlab-{version}-linux-x86_64', f'vmlab-fastpath-{version}-linux-x86_64',
                     f'vmlab-guest-{version}.tar.gz', f'vmlab-hv-{version}-linux-x86_64.tar.gz']:
            (dist / name).write_bytes(name.encode())
            if name.endswith('.tar.gz'):
                (dist / f'{name}.sha256').write_text('sidecar\n')
        subprocess.run(['python3', str(ROOT / 'scripts/release-assets.py'), 'prepare', str(dist),
                        version, '--commit', COMMIT, '--builder', 'helios'], check=True)
        return dist

    def calls(self):
        log = self.state / 'docker.log'
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def runs(self):
        return [call for call in self.calls() if call['args'][:1] == ['run']]


def shown(result):
    return result.stdout + result.stderr


# ── The key reaches cosign through the environment of docker run alone ──────

def test_signing_runs_cosign_in_the_container_with_the_key_in_the_environment_only(tmp_path):
    machine = Machine(tmp_path)
    dist = machine.release()
    result = machine.script('release-sign.sh', dist, 'key', **machine.secrets)
    assert result.returncode == 0, result.stderr
    assert (dist / 'SHA256SUMS.key.bundle').is_file()
    sign, verify = machine.runs()
    # `-e NAME` with no value: the name is in the argument list, the value is not.
    arguments = ' '.join(sign['args'])
    assert '-e VMLAB_RELEASE_COSIGN_KEY -e COSIGN_PASSWORD' in arguments
    for call in machine.calls():
        for secret in machine.secrets.values():
            assert all(secret not in argument for argument in call['args'])
    assert sign['env'] == machine.secrets
    # No network, the uid of the caller, removed at the end, the image of the build.
    for run in (sign, verify):
        assert run['args'][1:4] == ['--rm', '--network', 'none']
        assert f'{os.getuid()}:{os.getgid()}' in run['args']
        assert run['args'][run['args'].index('cosign') - 1] == 'vmlab-build'
    assert f'{dist}:{dist}' in sign['args']
    # Verifying mounts read-only and names no secret.
    assert f'{dist}:{dist}:ro' in verify['args'] and '-e' not in verify['args']
    # Nothing that the step prints holds a value, and no file of the machine does.
    for secret in machine.secrets.values():
        assert secret not in shown(result)
        for path in [*dist.iterdir(), *machine.checkout.rglob('*')]:
            assert not path.is_file() or secret.encode() not in path.read_bytes()


def test_a_shell_trace_of_the_signing_shows_no_value(tmp_path):
    machine = Machine(tmp_path)
    dist = machine.release()
    result = machine.script('release-sign.sh', dist, 'key', shell=('bash', '-x'),
                            **machine.secrets, SHELLOPTS='xtrace')
    assert result.returncode == 0, result.stderr
    for secret in machine.secrets.values():
        assert secret not in shown(result)


@pytest.mark.parametrize('missing', ['VMLAB_RELEASE_COSIGN_KEY', 'COSIGN_PASSWORD'])
@pytest.mark.parametrize('how', ['unset', 'empty'])
def test_a_missing_variable_is_named_and_no_value_is_shown(tmp_path, missing, how):
    machine = Machine(tmp_path)
    dist = machine.release()
    env = dict(machine.secrets)
    if how == 'unset':
        del env[missing]
    else:
        env[missing] = ''
    for script, args in (('release-sign.sh', [dist, 'key']),
                         ('release-cosign.sh', ['--rw', dist, '--secrets', '--', 'sign-blob'])):
        result = machine.script(script, *args, **env)
        assert result.returncode != 0
        assert f'{missing} is not set' in result.stderr
        for secret in machine.secrets.values():
            assert secret not in shown(result)
    assert not (dist / 'SHA256SUMS.key.bundle').exists()
    assert machine.runs() == []


def test_cosign_needs_docker_and_an_image_that_has_it(tmp_path):
    machine = Machine(tmp_path)
    dist = machine.release()
    result = machine.script('release-sign.sh', dist, 'key', **machine.secrets, STUB_COSIGN_LABEL='')
    assert result.returncode != 0
    assert 'the image vmlab-build has none' in result.stderr and 'just buildbox::image' in result.stderr
    result = machine.script('release-sign.sh', dist, 'key', **machine.secrets, STUB_NO_IMAGE='1')
    assert 'there is no image vmlab-build' in result.stderr
    assert machine.runs() == []
    # Another image name is taken from the environment of the build.
    result = machine.script('release-sign.sh', dist, 'key', **machine.secrets, VMLAB_BUILD_IMAGE='other-image')
    assert result.returncode == 0, result.stderr
    assert 'other-image' in machine.runs()[0]['args']
    (machine.tools / 'docker').unlink()
    result = machine.script('release-cosign.sh', '--check', PATH='/usr/bin:/bin')
    if not any((Path(d) / 'docker').exists() for d in ('/usr/bin', '/bin')):
        assert result.returncode != 0 and 'no docker on PATH' in result.stderr


def test_the_runner_refuses_paths_it_cannot_mount(tmp_path):
    machine = Machine(tmp_path)
    for args, reason in ((['--rw', 'relative', '--', 'version'], 'absolute path'),
                         (['--rw', '/', '--', 'version'], 'does not take /'),
                         (['--ro', tmp_path / 'absent', '--', 'version'], 'no such path'),
                         (['--rw', tmp_path / 'tools/docker', '--', 'version'], 'takes a directory'),
                         ([], 'usage')):
        result = machine.script('release-cosign.sh', *args)
        assert result.returncode != 0 and reason in result.stderr, result.stderr
    assert machine.runs() == []


# ── release::verify ─────────────────────────────────────────────────────────

def test_verify_checks_the_signature_in_the_container(tmp_path):
    machine = Machine(tmp_path)
    dist = machine.release()
    result = machine.script('release-verify.sh', dist)
    assert result.returncode == 0, result.stderr
    assert 'UNSIGNED' in result.stderr and machine.runs() == []
    assert machine.script('release-sign.sh', dist, 'key', **machine.secrets).returncode == 0
    result = machine.script('release-verify.sh', dist)
    assert result.returncode == 0, result.stderr
    assert 'SHA256SUMS.key.bundle verifies' in result.stderr
    # A bundle of another key pair is refused, here and at the upload.
    (dist / 'SHA256SUMS.key.bundle').write_text(
        (dist / 'SHA256SUMS.key.bundle').read_text().replace(pair_of(PUBLIC), pair_of(OTHER_PUBLIC)))
    result = machine.script('release-verify.sh', dist)
    assert result.returncode != 0 and 'does not verify' in result.stderr
    result = machine.script('release-publish.sh', dist, '--print')
    assert result.returncode != 0 and 'does not verify' in result.stderr
    # A test key verifies with its own public half, which the upload never takes.
    other = tmp_path / 'other.pub'
    other.write_text(OTHER_PUBLIC)
    assert machine.script('release-verify.sh', dist, VMLAB_RELEASE_PUBLIC_KEY=str(other)).returncode == 0
    result = machine.script('release-publish.sh', dist, '--print', VMLAB_RELEASE_PUBLIC_KEY=str(other))
    assert result.returncode != 0
    # An empty bundle is not an unsigned release, and a changed asset fails first.
    (dist / 'SHA256SUMS.key.bundle').write_text('')
    assert 'is empty' in machine.script('release-verify.sh', dist).stderr
    (dist / 'vmlab-1.2.3-linux-x86_64').write_bytes(b'replaced')
    assert machine.script('release-verify.sh', dist).returncode != 0


def test_verify_needs_a_public_half_for_a_signed_release(tmp_path):
    machine = Machine(tmp_path, committed=None)
    dist = machine.release()
    (dist / 'SHA256SUMS.key.bundle').write_text('a bundle')
    result = machine.script('release-verify.sh', dist)
    assert result.returncode != 0 and 'no public half' in result.stderr


# ── release::keygen ─────────────────────────────────────────────────────────

# cosign generate-key-pair by contract: the two files in the working directory.
STUB_KEYGEN = f'''#!/bin/sh
set -eu
[ "$*" = generate-key-pair ]
[ -z "${{COSIGN_PASSWORD+set}}" ] || {{ echo "a password was inherited" >&2; exit 1; }}
umask 022
cat > cosign.key <<'KEY'
{PRIVATE}KEY
cat > cosign.pub <<'KEY'
{PUBLIC}KEY
'''


def keygen(machine, directory, **env):
    stub = machine.tools / 'cosign'
    stub.write_text(STUB_KEYGEN)
    stub.chmod(0o755)
    return machine.script('release-keygen.sh', directory, COSIGN=str(stub), **env)


def test_keygen_writes_the_pair_outside_the_checkout_and_says_what_is_next(tmp_path):
    machine = Machine(tmp_path)
    keys = tmp_path / 'keys/release'
    # A password in the environment is not handed on: the operator types it.
    result = keygen(machine, keys, COSIGN_PASSWORD=SECRET_PASSWORD)
    assert result.returncode == 0, result.stderr
    assert stat.S_IMODE((keys / 'cosign.key').stat().st_mode) == 0o600
    assert stat.S_IMODE((keys / 'cosign.pub').stat().st_mode) == 0o644
    assert stat.S_IMODE(keys.stat().st_mode) == 0o700
    for words in ('VMLAB_RELEASE_COSIGN_KEY', 'COSIGN_PASSWORD', 'Keep a copy', 'just release::trust',
                  'Remove'):
        assert words in result.stderr
    assert SECRET_PASSWORD not in shown(result) and 'Zml4dHVyZQ' not in shown(result)
    # A key is never overwritten.
    before = (keys / 'cosign.key').read_bytes()
    result = keygen(machine, keys)
    assert result.returncode != 0 and 'exists already' in result.stderr
    assert (keys / 'cosign.key').read_bytes() == before


def test_keygen_refuses_a_directory_that_git_can_add(tmp_path):
    machine = Machine(tmp_path)
    inside = machine.checkout / 'target/keys'
    link = tmp_path / 'link'
    link.symlink_to(machine.checkout)
    for directory in (inside, machine.checkout, link / 'keys'):
        result = keygen(machine, directory)
        assert result.returncode != 0 and 'inside the checkout' in result.stderr, result.stderr
    assert not inside.exists() and not (machine.checkout / 'keys').exists()
    assert not (machine.checkout / 'cosign.key').exists()
    other = tmp_path / 'other-repo'
    other.mkdir()
    subprocess.run(['git', '-C', str(other), 'init', '-q'], check=True)
    result = keygen(machine, other / 'keys')
    assert result.returncode != 0 and 'inside the git checkout' in result.stderr
    assert not (other / 'keys/cosign.key').exists()


def test_keygen_needs_a_terminal_in_the_container(tmp_path):
    machine = Machine(tmp_path)
    result = machine.script('release-keygen.sh', tmp_path / 'keys')
    assert result.returncode != 0 and 'run it in a terminal' in result.stderr
    assert machine.runs() == []


# ── release::trust ──────────────────────────────────────────────────────────

def trust(machine, *args):
    return machine.script('release-trust.sh', *args)


def embedded(machine):
    return re.search(r"^RELEASE_KEY='([^']*)'$", (machine.checkout / 'install.sh').read_text(), re.M)[1]


def test_trust_writes_the_public_key_to_both_places(tmp_path):
    machine = Machine(tmp_path, committed=None)
    committed = machine.checkout / 'build/release-signing.pub'
    assert trust(machine, '--check').returncode == 0
    assert embedded(machine) == ''
    mode = (machine.checkout / 'install.sh').stat().st_mode
    pub = tmp_path / 'cosign.pub'
    pub.write_text(PUBLIC.replace('\n', '\r\n') + '\n')
    result = trust(machine, pub)
    assert result.returncode == 0, result.stderr
    assert committed.read_text() == PUBLIC
    assert embedded(machine) == PUBLIC.strip()
    assert (machine.checkout / 'install.sh').stat().st_mode == mode
    assert 'hold the same public key' in result.stderr and 'commit' in result.stderr
    assert trust(machine, '--check').returncode == 0
    # Only the switch changed in install.sh, and the installer still parses.
    assert (machine.checkout / 'install.sh').read_text() == machine.before_trust.replace(
        "\nRELEASE_KEY=''\n", f"\nRELEASE_KEY='{PUBLIC.strip()}'\n")
    assert subprocess.run(['sh', '-n', str(machine.checkout / 'install.sh')]).returncode == 0
    # A second key replaces the first in both places.
    pub.write_text(OTHER_PUBLIC)
    assert trust(machine, pub).returncode == 0
    assert committed.read_text() == OTHER_PUBLIC and embedded(machine) == OTHER_PUBLIC.strip()


def test_the_check_fails_when_the_two_places_differ(tmp_path):
    machine = Machine(tmp_path)
    result = trust(machine, '--check')
    assert result.returncode != 0 and 'trusts no key' in result.stderr
    pub = tmp_path / 'cosign.pub'
    pub.write_text(PUBLIC)
    assert trust(machine, pub).returncode == 0
    (machine.checkout / 'build/release-signing.pub').write_text(OTHER_PUBLIC)
    result = trust(machine, '--check')
    assert result.returncode != 0 and 'holds another key' in result.stderr
    (machine.checkout / 'build/release-signing.pub').unlink()
    result = trust(machine, '--check')
    assert result.returncode != 0 and 'is missing' in result.stderr


def test_trust_takes_a_public_key_and_nothing_else(tmp_path):
    machine = Machine(tmp_path, committed=None)
    before = (machine.checkout / 'install.sh').read_bytes()
    for name, text, reason in (('cosign.key', PRIVATE, 'holds a private key'),
                               ('both.pem', PUBLIC + PRIVATE, 'holds a private key'),
                               ('text.pub', 'not a key\n', 'not one PEM public key'),
                               ('quote.pub', PUBLIC.replace('MFkw', "MF'w"), 'not one PEM public key'),
                               ('two.pub', PUBLIC + PUBLIC, 'not one PEM public key')):
        path = tmp_path / name
        path.write_text(text)
        result = trust(machine, path)
        assert result.returncode != 0 and reason in result.stderr, result.stderr
        assert 'Zml4dHVyZQ' not in shown(result)
    assert trust(machine, tmp_path / 'absent.pub').returncode != 0
    assert (machine.checkout / 'install.sh').read_bytes() == before
    assert not (machine.checkout / 'build/release-signing.pub').exists()


def test_this_checkout_has_one_public_key():
    """What `just ci::release-dry-run` runs first: both places hold the same
    key. The switch is on since card 1030, so the check must not say it is off."""
    result = subprocess.run(['bash', str(ROOT / 'scripts/release-trust.sh'), '--check'],
                            capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    assert 'hold the same public key' in result.stderr
    committed = (ROOT / 'build/release-signing.pub').read_text()
    assert committed.strip() == re.search(r"^RELEASE_KEY='([^']*)'$", (ROOT / 'install.sh').read_text(), re.M)[1]
    assert 'PRIVATE' not in committed


# ── release::doctor ─────────────────────────────────────────────────────────

def test_the_doctor_says_whether_the_variables_are_set_and_never_their_values(tmp_path):
    machine = Machine(tmp_path)
    pub = tmp_path / 'cosign.pub'
    pub.write_text(PUBLIC)
    assert trust(machine, pub).returncode == 0
    result = machine.script('release-doctor.sh', 'key', shell=('bash', '-x'), **machine.secrets)
    for name in machine.secrets:
        assert f'ok       {name} is set' in result.stdout
    for secret in machine.secrets.values():
        assert secret not in shown(result)
    assert 'ok       cosign v2.6.1 in the image vmlab-build' in result.stdout
    assert 'install.sh trusts the same key' in result.stdout
    assert all(call['args'][:1] != ['run'] for call in machine.calls())
    # Without the variables: a note for an unsigned build, missing for a signed one.
    result = machine.script('release-doctor.sh')
    assert 'note     VMLAB_RELEASE_COSIGN_KEY is not set' in result.stdout
    assert 'MISSING  COSIGN_PASSWORD' not in result.stdout
    result = machine.script('release-doctor.sh', VMLAB_RELEASE_SIGNING='key')
    assert result.returncode == 1
    for name in machine.secrets:
        assert f'MISSING  {name} is not set' in result.stdout


def test_the_doctor_names_what_a_machine_lacks(tmp_path):
    machine = Machine(tmp_path, committed=None)
    result = machine.script('release-doctor.sh', 'key', STUB_COSIGN_LABEL='', VMLAB_RELEASE_MIN_FREE_GIB='99999999')
    assert result.returncode == 1
    assert 'MISSING  cosign in the image vmlab-build' in result.stdout
    assert 'MISSING  disk space' in result.stdout
    assert 'MISSING  no public key in the repo yet' in result.stdout
    assert 'MISSING  a git checkout' in result.stdout
    result = machine.script('release-doctor.sh', STUB_NO_IMAGE='1')
    assert 'note     no image vmlab-build yet' in result.stdout
    (machine.tools / 'docker').write_text('#!/bin/sh\nexit 1\n')
    result = machine.script('release-doctor.sh')
    assert result.returncode == 1 and 'cannot reach the daemon' in result.stdout
    assert machine.script('release-doctor.sh', 'keyless').returncode == 2
    # A machine without git and python3: each is named once, and no check that
    # needs one gives another reason.
    bare = tmp_path / 'bare'
    bare.mkdir()
    for tool in ('bash', 'sed', 'head', 'hostname', 'uname', 'df', 'awk', 'dirname', 'sha256sum', 'od'):
        found = subprocess.run(['bash', '-c', f'command -v {tool}'], capture_output=True, text=True).stdout.strip()
        (bare / tool).symlink_to(found)
    result = machine.script('release-doctor.sh', 'key', shell=(str(bare / 'bash'),), PATH=str(bare))
    assert result.returncode == 1
    for tool in ('git', 'python3', 'docker'):
        assert result.stdout.count(f'MISSING  {tool} on PATH') == 1
    assert 'there is no git' in result.stdout and 'there is no python3' in result.stdout
    assert 'install.sh trusts a key' not in result.stdout and 'a git checkout at' not in result.stdout


# ── The image ───────────────────────────────────────────────────────────────

def test_the_build_image_holds_the_pinned_cosign_and_no_key():
    dockerfile = (ROOT / 'build/Dockerfile').read_text()
    pin = re.search(r'^VERSION=(v[0-9.]+)$', (ROOT / 'scripts/install-cosign.sh').read_text(), re.M)[1]
    assert tuple(map(int, pin[1:].split('.'))) >= (2, 6, 1)
    assert re.search(r'^SHA256=[0-9a-f]{64}$', (ROOT / 'scripts/install-cosign.sh').read_text(), re.M)
    assert 'RUN ./scripts/install-cosign.sh /usr/local/bin' in dockerfile
    assert f'LABEL io.vmlab.cosign="{pin}"' in dockerfile
    assert f"GitVersion: *{pin}$".replace('.', r'\.') in dockerfile
    assert '!scripts/install-cosign.sh' in (ROOT / 'build/Dockerfile.dockerignore').read_text()
    # No instruction of the image takes a key or a password.
    for word in ('COSIGN_PASSWORD', 'VMLAB_RELEASE_COSIGN_KEY', 'cosign.key', '--mount=type=secret'):
        assert word not in dockerfile
    # The build containers of build/run.sh get an explicit list of variables.
    run = (ROOT / 'build/run.sh').read_text()
    assert 'COSIGN' not in run and '--env-file' not in run
