"""ci.yml builds and publishes no version; the release is local (card 423)."""
import json
import os
from pathlib import Path
import re
import subprocess

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
CI = (ROOT / '.github/workflows/ci.yml').read_text()
WORKFLOW = yaml.load(CI, Loader=yaml.BaseLoader)
JOBS = WORKFLOW['jobs']
COMMIT = 'c' * 40


def test_ci_has_only_the_cheap_checks():
    assert set(JOBS) == {'release-dry-run', 'fmt-lint', 'check', 'hv-check',
                         'guest-crates', 'ebpf-verify'}
    for name in ['version', 'build', 'vdisplay-build', 'vdisplay-sign', 'guest-assets',
                 'hv-build', 'release-assets', 'release']:
        assert name not in JOBS


def test_ci_cannot_sign_or_publish():
    assert WORKFLOW['permissions'] == {'contents': 'read'}
    for job in JOBS.values():
        assert 'permissions' not in job
        assert 'do_release' not in job.get('if', '')
    # Nothing can run on a second trigger, and no step reaches a release.
    assert set(WORKFLOW['on']) == {'push', 'pull_request'}
    code = '\n'.join(line for line in CI.splitlines() if not line.lstrip().startswith('#'))
    for word in ['cosign', 'id-token', 'gh release', 'action-gh-release', 'upload-artifact',
                 'release-version.sh', 'release-assets.py', 'guest-package', 'hv-package',
                 'contents: write']:
        assert word not in code, word


def test_no_other_workflow_publishes_a_vmlab_version():
    for path in (ROOT / '.github/workflows').glob('*.yml'):
        text = path.read_text()
        assert 'action-gh-release' not in text, path.name
        assert 'release-assets.py' not in text, path.name


def release_dir(tmp_path, version):
    dist = tmp_path / 'dist'
    dist.mkdir()
    for name in [f'vmlab-{version}-linux-x86_64', f'vmlab-fastpath-{version}-linux-x86_64',
                 f'vmlab-guest-{version}.tar.gz', f'vmlab-hv-{version}-linux-x86_64.tar.gz']:
        (dist / name).write_bytes(b'fixture ' + name.encode())
    for name in [f'vmlab-guest-{version}.tar.gz', f'vmlab-hv-{version}-linux-x86_64.tar.gz']:
        (dist / (name + '.sha256')).write_text('checksum fixture\n')
    subprocess.run(['python3', str(ROOT / 'scripts/release-assets.py'), 'prepare', str(dist),
                    version, '--commit', COMMIT, '--builder', 'helios'], check=True)
    return dist


def publish(dist, *args, env=None):
    return subprocess.run(['bash', str(ROOT / 'scripts/release-publish.sh'), str(dist), *args],
                          capture_output=True, text=True, env=env)


# A cosign that signs and verifies by contract. A private half is the text
# `private of <public half>`, and a bundle names the public half that verifies
# it and the manifest it signs. It proves no cryptography.
STUB_COSIGN = r"""#!/bin/sh
set -eu
case "$1" in
  sign-blob)
    printf '%s %s\n' "${VMLAB_RELEASE_COSIGN_KEY#private of }" "$(sha256sum < "$8")" > "$7" ;;
  verify-blob)
    [ "$2" = --key ] && [ "$4" = --bundle ] && [ "$6" = --insecure-ignore-tlog=true ]
    [ "$(cat "$5")" = "$(cat "$3") $(sha256sum < "$7")" ] ;;
  *) exit 1 ;;
esac
"""


class Signing:
    """A checkout with the release scripts, a committed public half and stub tools."""

    def __init__(self, tmp_path, committed='release-key'):
        self.tmp = tmp_path
        self.checkout = tmp_path / 'checkout'
        for name in ['scripts/release-publish.sh', 'scripts/release-sign.sh', 'scripts/release-assets.py',
                     'scripts/release-cosign.sh', 'scripts/release-verify.sh']:
            (self.checkout / name).parent.mkdir(parents=True, exist_ok=True)
            (self.checkout / name).write_bytes((ROOT / name).read_bytes())
        if committed is not None:
            (self.checkout / 'build').mkdir()
            (self.checkout / 'build/release-signing.pub').write_text(committed + '\n')
        tools = tmp_path / 'tools'
        tools.mkdir()
        (tools / 'cosign').write_text(STUB_COSIGN)
        (tools / 'gh').write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$GH_LOG"\n')
        for tool in tools.iterdir():
            tool.chmod(0o755)
        self.gh_log = tmp_path / 'gh.log'
        self.env = {k: v for k, v in os.environ.items()
                    if k != 'COSIGN' and not k.startswith('VMLAB_')}
        # COSIGN names the stub: without it the scripts run cosign in the build
        # container (scripts/release-cosign.sh).
        self.env.update(PATH=f"{tools}:{os.environ['PATH']}", GH_LOG=str(self.gh_log),
                        COSIGN=str(tools / 'cosign'))

    def sign(self, dist, key, public=None):
        """Sign with the private half of `key`; `public` is the test override of release-sign.sh."""
        env = dict(self.env, VMLAB_RELEASE_COSIGN_KEY=f'private of {key}', COSIGN_PASSWORD='the password')
        if public is not None:
            other = self.tmp / 'test.pub'
            other.write_text(public + '\n')
            env['VMLAB_RELEASE_PUBLIC_KEY'] = str(other)
        result = subprocess.run(['bash', str(self.checkout / 'scripts/release-sign.sh'), str(dist), 'key'],
                                capture_output=True, text=True, env=env)
        assert result.returncode == 0, result.stderr
        assert (dist / 'SHA256SUMS.key.bundle').is_file()

    def publish(self, dist, *args, **env):
        return subprocess.run(['bash', str(self.checkout / 'scripts/release-publish.sh'), str(dist), *args],
                              capture_output=True, text=True, env=dict(self.env, **env))

    def refused(self, dist, reason, **env):
        """Neither --print nor the upload goes on, and gh never runs."""
        for args in (['--print'], []):
            result = self.publish(dist, *args, **env)
            assert result.returncode != 0, result.stdout
            assert reason in result.stderr, result.stderr
            assert result.stdout == ''
            assert not self.gh_log.exists()


@pytest.mark.parametrize('version,prerelease', [('1.2.3-alpha', True), ('1.2.3', False)])
def test_publish_prints_the_one_upload_command(tmp_path, version, prerelease):
    signing = Signing(tmp_path)
    dist = release_dir(tmp_path, version)
    signing.sign(dist, 'release-key')
    result = signing.publish(dist, '--print')
    assert result.returncode == 0, result.stderr
    words = result.stdout.split()
    assert words[:9] == ['gh', 'release', 'create', f'v{version}', '--repo', 'VMLabDev/vmlab',
                         '--target', COMMIT, '--title']
    assert ('--prerelease' in words) == prerelease
    uploaded = sorted(Path(word).name for word in words if word.startswith(str(dist)))
    assert uploaded == sorted(path.name for path in dist.iterdir())
    assert 'SHA256SUMS.key.bundle' in uploaded
    assert 'UNSIGNED' not in result.stderr
    # The upload itself takes the same bundle.
    result = signing.publish(dist)
    assert result.returncode == 0, result.stderr
    assert str(dist / 'SHA256SUMS.key.bundle') in signing.gh_log.read_text().splitlines()


def test_publish_refuses_a_signature_of_a_test_key(tmp_path):
    """Review install-release F8, first way: release-sign.sh verifies with
    VMLAB_RELEASE_PUBLIC_KEY when it is set, so a test pair passes signing."""
    signing = Signing(tmp_path)
    dist = release_dir(tmp_path, '1.2.3')
    signing.sign(dist, 'test-key', public='test-key')
    signing.refused(dist, 'does not verify')
    # The override of the signing step is no way around the check.
    signing.refused(dist, 'does not verify', VMLAB_RELEASE_PUBLIC_KEY=str(tmp_path / 'test.pub'))


def test_publish_refuses_a_signature_of_an_earlier_manifest(tmp_path):
    """Review install-release F8, second way: `prepare` writes a new
    provenance.json and SHA256SUMS and leaves the bundle of the earlier one."""
    signing = Signing(tmp_path)
    dist = release_dir(tmp_path, '1.2.3')
    signing.sign(dist, 'release-key')
    assert signing.publish(dist, '--print').returncode == 0
    # A build of another commit into the same directory: a new manifest.
    subprocess.run(['python3', str(ROOT / 'scripts/release-assets.py'), 'prepare', str(dist),
                    '1.2.3', '--commit', 'd' * 40, '--builder', 'helios'], check=True)
    signing.refused(dist, 'does not verify')


def test_publish_refuses_a_signature_it_cannot_check(tmp_path):
    signing = Signing(tmp_path, committed=None)
    dist = release_dir(tmp_path, '1.2.3')
    signing.sign(dist, 'test-key', public='test-key')
    signing.refused(dist, 'no public half')
    # An empty bundle is not an unsigned release.
    (dist / 'SHA256SUMS.key.bundle').write_text('')
    signing.refused(dist, 'is empty')
    with_key = tmp_path / 'with-key'
    with_key.mkdir()
    signing = Signing(with_key)
    signing.sign(dist, 'release-key')
    signing.refused(dist, 'need cosign', COSIGN=str(tmp_path / 'no-cosign'))


def test_publish_only_uploads(tmp_path):
    dist = release_dir(tmp_path, '1.2.3-alpha')
    before = {path.name: path.read_bytes() for path in dist.iterdir()}
    tools = tmp_path / 'tools'
    tools.mkdir()
    (tools / 'gh').write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$GH_LOG"\n')
    (tools / 'gh').chmod(0o755)
    env = dict(os.environ, PATH=f"{tools}:{os.environ['PATH']}", GH_LOG=str(tmp_path / 'gh.log'))
    result = publish(dist, env=env)
    assert result.returncode == 0, result.stderr
    assert 'UNSIGNED' in result.stderr
    arguments = (tmp_path / 'gh.log').read_text().splitlines()
    assert arguments[:3] == ['release', 'create', 'v1.2.3-alpha']
    assert '--prerelease' in arguments
    assert not any(argument.endswith('.bundle') for argument in arguments)
    assert {path.name: path.read_bytes() for path in dist.iterdir()} == before


def test_publish_refuses_a_dry_run_and_a_changed_directory(tmp_path):
    result = publish(release_dir(tmp_path, '0.0.0-dry-run'), '--print')
    assert result.returncode != 0
    assert 'dry run' in result.stderr
    other = tmp_path / 'other'
    other.mkdir()
    dist = release_dir(other, '1.2.3')
    (dist / 'vmlab-1.2.3-linux-x86_64').write_bytes(b'replaced after the build')
    result = publish(dist, '--print')
    assert result.returncode != 0
    assert result.stdout == ''


def test_guest_dry_run_needs_only_the_dry_run_version():
    package = (ROOT / 'guest/package.sh').read_text()
    assert "[ \"$version\" = 0.0.0-dry-run ] || die 'dry-run requires the dry-run version'" in package
    assert 'GITHUB_' not in package


def test_action_pins_remain_immutable():
    for job in JOBS.values():
        for step in job['steps']:
            if 'uses' in step:
                assert re.fullmatch(r'[^@]+@[0-9a-f]{40}', step['uses'])


@pytest.mark.parametrize('unsigned', [False, True])
def test_display_package_requires_signing_unless_explicit_dry_run(tmp_path, unsigned):
    package = tmp_path / 'package'
    package.mkdir()
    names = ['vmlab-display.inf', 'vmlab-display.dll', 'install.exe', 'install.ps1',
             'ivshmem/ivshmem.inf', 'ivshmem/ivshmem.cat', 'ivshmem/ivshmem.sys',
             'LICENSE-Microsoft.txt', 'LICENSE-IVSHMEM.txt']
    for name in names:
        path = package / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b'compiled fixture')
    (package / 'BUILDINFO.json').write_text(json.dumps({'sources': {}}))
    output = tmp_path / 'output'
    command = ['python3', str(ROOT / 'guest/vdisplay-windows/package.py'),
               str(package), str(output)]
    if unsigned:
        command.append('--unsigned-dry-run')
    result = subprocess.run(command, capture_output=True, text=True)
    if unsigned:
        assert result.returncode == 0, result.stderr
        assert (output / 'vmlab-display.dll').is_file()
        assert not (output / 'vmlab-display.cat').exists()
        assert not (output / 'vmlab-display.cer').exists()
    else:
        assert result.returncode != 0
        assert not output.exists()


# A docker that keeps each named volume as a directory. `docker run` lists what
# every mounted volume holds before the command, and then writes one file into
# each: the stand-in for a build.rs that edits a crate source in the cargo
# home, or leaves an output in a target dir.
STUB_DOCKER = r'''#!/usr/bin/env python3
import json, os, shutil, sys
from pathlib import Path

state = Path(os.environ['STUB_DOCKER_STATE'])
volumes = state / 'volumes'
volumes.mkdir(parents=True, exist_ok=True)
args = sys.argv[1:]
if args[:2] == ['image', 'inspect']:
    if '-f' in args:
        print('sha256:' + '0' * 64)
elif args[:2] == ['volume', 'ls']:
    print(''.join(name + '\n' for name in sorted(os.listdir(volumes))), end='')
elif args[:2] == ['volume', 'rm']:
    for name in args[2:]:
        shutil.rmtree(volumes / name)
elif args[:1] == ['run']:
    mounted = {}
    for flag, value in zip(args, args[1:]):
        name = value.split(':')[0]
        if flag == '-v' and '/' not in name:
            volume = volumes / name
            volume.mkdir(exist_ok=True)
            mounted[name] = sorted(os.listdir(volume))
            (volume / 'left-by-a-build').write_text(os.environ['STUB_DOCKER_BUILD'])
    with open(state / 'runs', 'a') as runs:
        runs.write(json.dumps({'build': os.environ['STUB_DOCKER_BUILD'], 'volumes': mounted}) + '\n')
else:
    sys.exit('stub docker: unexpected ' + ' '.join(args))
'''


def release_checkout(tmp_path, docker=STUB_DOCKER,
                     scripts=('build/run.sh', 'scripts/release-local.sh')):
    """A committed checkout with the release scripts and a `.gitignore`, and the
    environment in which `docker` is the stub. Returns (checkout, env, state)."""
    checkout = tmp_path / 'checkout'
    for name in scripts:
        (checkout / name).parent.mkdir(parents=True, exist_ok=True)
        (checkout / name).write_bytes((ROOT / name).read_bytes())
        (checkout / name).chmod(0o755)
    (checkout / '.gitignore').write_text('/target\n/.ci-cache/\n')
    git = ['git', '-C', str(checkout), '-c', 'user.name=t', '-c', 'user.email=t@example.invalid']
    subprocess.run([*git, 'init', '-q'], check=True)
    subprocess.run([*git, 'add', '-A'], check=True)
    subprocess.run([*git, 'commit', '-q', '-m', 'release commit'], check=True)

    bin_dir = tmp_path / 'bin'
    bin_dir.mkdir()
    (bin_dir / 'docker').write_text(docker)
    (bin_dir / 'docker').chmod(0o755)
    state = tmp_path / 'docker-state'
    env = {k: v for k, v in os.environ.items() if not k.startswith('VMLAB_')}
    env.update(PATH=f"{bin_dir}:{env['PATH']}", STUB_DOCKER_STATE=str(state),
               STUB_DOCKER_BUILD='release')
    return checkout, env, state


def release_dry_run(checkout, env):
    return subprocess.run([str(checkout / 'scripts/release-local.sh'), '--version', '0.0.0-dry-run',
                           '--display', 'unsigned', '--image-as-is'], capture_output=True, text=True,
                          env=env)


@pytest.mark.parametrize('hidden', ['', 'info-exclude', 'own-gitignore', 'parent-gitignore',
                                    'global-exclude'])
def test_a_release_refuses_an_untracked_file(tmp_path, hidden):
    """Review install-release F7: the container builds from the whole checkout,
    so a file that HEAD does not hold (a `.cargo/config.toml` with `[env]` or
    `[patch]`) changes the assets while the provenance names HEAD. No exclude
    that is not in the commit may hide the file: `.git/info/exclude`, the
    user's `core.excludesFile`, or an untracked `.gitignore` that ignores
    itself with the directory."""
    checkout, env, state = release_checkout(tmp_path)
    (checkout / '.cargo').mkdir()
    (checkout / '.cargo/config.toml').write_text('[env]\nVMLAB_PROBE = "x"\n')
    if hidden == 'info-exclude':
        (checkout / '.git/info').mkdir(exist_ok=True)
        (checkout / '.git/info/exclude').write_text('.cargo/\n')
    elif hidden == 'own-gitignore':
        (checkout / '.cargo/.gitignore').write_text('*\n')
    elif hidden == 'parent-gitignore':
        # The tracked root .gitignore is unchanged: a second one, one level
        # down, hides itself and the config below it.
        (checkout / '.cargo').rename(checkout / 'cfg')
        (checkout / 'build/.cargo').mkdir()
        (checkout / 'cfg/config.toml').rename(checkout / 'build/.cargo/config.toml')
        (checkout / 'cfg').rmdir()
        (checkout / 'build/.gitignore').write_text('.gitignore\n.cargo/\n')
    elif hidden == 'global-exclude':
        excludes = tmp_path / 'global-excludes'
        excludes.write_text('.cargo/\n')
        env.update(GIT_CONFIG_COUNT='1', GIT_CONFIG_KEY_0='core.excludesFile',
                   GIT_CONFIG_VALUE_0=str(excludes))

    release = release_dry_run(checkout, env)
    assert release.returncode != 0
    assert 'untracked' in release.stderr, release.stderr
    named = 'build/.gitignore' if hidden == 'parent-gitignore' else '.cargo/'
    assert f'\n  {named}' in release.stderr, release.stderr
    # Refused before any container ran.
    assert not (state / 'runs').exists()


def test_a_release_accepts_what_the_commit_ignores(tmp_path):
    """The build outputs under a path that a committed `.gitignore` names are
    not a refusal: an earlier release leaves `target/` in the checkout. A
    `.gitignore` there is no refusal either, the tracked one covers it."""
    checkout, env, state = release_checkout(tmp_path)
    (checkout / 'target/dist').mkdir(parents=True)
    (checkout / 'target/dist/left-by-an-earlier-build').write_text('x')
    (checkout / 'target/dist/.gitignore').write_text('*\n')

    release = release_dry_run(checkout, env)
    # The stub builds nothing, so the release stops where it collects the assets.
    assert 'the build left no' in release.stderr, release.stderr
    assert (state / 'runs').exists()


def test_a_release_build_takes_nothing_from_the_volumes_of_another_build(tmp_path):
    """Review install-release F3: a card build writes to the cargo home and the
    target volumes; a release from the same checkout path must not read them."""
    checkout, env, state = release_checkout(tmp_path)

    def runs():
        return [json.loads(line) for line in (state / 'runs').read_text().splitlines()]

    card = subprocess.run([str(checkout / 'build/run.sh'), 'true'], capture_output=True, text=True,
                          env={**env, 'STUB_DOCKER_BUILD': 'card'})
    assert card.returncode == 0, card.stderr
    card_volumes = set(runs()[0]['volumes'])
    assert any(name.endswith('-cargo') for name in card_volumes)
    assert len(card_volumes) > 1

    # The stub builds nothing, so the release stops where it collects the
    # assets. Both of its containers (display driver, dist) have run by then.
    release = release_dry_run(checkout, env)
    assert 'the build left no' in release.stderr, release.stderr
    release_runs = [run for run in runs() if run['build'] == 'release']
    assert len(release_runs) == 2
    first, second = (run['volumes'] for run in release_runs)
    assert len(first) == len(card_volumes)
    # No volume of the card build, and nothing in a volume at the start.
    assert not card_volumes & set(first)
    assert all(files == [] for files in first.values()), first
    # The second container of the release continues in the volumes of the first.
    assert set(second) == set(first)
    # The release leaves no volume behind, and the card build keeps its own.
    assert set(os.listdir(state / 'volumes')) == card_volumes


# ── Review install-release-2 F12 and F13 ─────────────────────────────────────

IMAGE_A = 'sha256:' + 'a' * 64
IMAGE_B = 'sha256:' + 'b' * 64

# A docker with one tag, `vmlab-build`, that the stub can move. `docker build`
# makes the image A and points the tag at it. With STUB_RETAG set, the first
# `docker run` moves the tag to the image B: another checkout on the same host
# ran `just buildbox::image` while the release built. Every `run` logs the
# image that it names. The run of build/dist.sh leaves the four assets, and the
# cosign runs sign and verify by contract, as the stub of test_release_key.py.
STUB_DOCKER_IMAGES = r"""#!/usr/bin/env python3
import hashlib, json, os, sys
from pathlib import Path

A, B = 'sha256:' + 'a' * 64, 'sha256:' + 'b' * 64
state = Path(os.environ['STUB_DOCKER_STATE'])
state.mkdir(parents=True, exist_ok=True)
tag = state / 'tag'
args = sys.argv[1:]
if args[:1] == ['build']:
    tag.write_text(A)
    if '--iidfile' in args:
        Path(args[args.index('--iidfile') + 1]).write_text(A)
elif args[:2] == ['image', 'inspect']:
    name = args[-1]
    if not name.startswith('sha256:'):
        if not tag.exists():
            sys.exit(1)
        name = tag.read_text()
    if '-f' in args:
        print('v2.6.1' if 'Labels' in args[args.index('-f') + 1] else name)
elif args[:2] == ['volume', 'ls']:
    pass
elif args[:1] == ['run']:
    images = [word for word in args if word == 'vmlab-build' or word.startswith('sha256:')]
    with open(state / 'runs', 'a') as runs:
        runs.write(json.dumps({'image': images[0] if images else None,
                               'tag': tag.read_text() if tag.exists() else None,
                               'secrets': '-e VMLAB_RELEASE_COSIGN_KEY' in ' '.join(args)}) + '\n')
    if os.environ.get('STUB_RETAG'):
        tag.write_text(B)
    digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
    if './build/dist.sh' in args:
        version = args[args.index('./build/dist.sh') + 1]
        dist = Path(args[args.index('-w') + 1]) / 'target/dist'
        dist.mkdir(parents=True, exist_ok=True)
        cli = dist / f'vmlab-{version}-linux-x86_64'
        cli.write_text(f'#!/bin/sh\necho "vmlab {version}"\n')
        cli.chmod(0o755)
        (dist / f'vmlab-fastpath-{version}-linux-x86_64').write_text('helper\n')
        for name in (f'vmlab-guest-{version}.tar.gz', f'vmlab-hv-{version}-linux-x86_64.tar.gz'):
            (dist / name).write_text(name)
            (dist / (name + '.sha256')).write_text(f'{digest(dist / name)}  {name}\n')
    elif 'cosign' in args:
        cosign = args[args.index('cosign') + 1:]
        if cosign[0] == 'sign-blob':
            Path(cosign[6]).write_text(f"pair {digest(cosign[7])}\n")
        elif cosign[0] == 'verify-blob':
            sys.exit(0 if Path(cosign[4]).read_text() == f"pair {digest(cosign[6])}\n" else 1)
        else:
            sys.exit('stub docker: unexpected cosign ' + ' '.join(cosign))
else:
    sys.exit('stub docker: unexpected ' + ' '.join(args))
"""


def signed_release(tmp_path, *args, **extra):
    """`release-local.sh --version 9.9.9 --signing key` of a fixture checkout
    with a stub key and a staged display package. Returns (result, runs, out)."""
    checkout, env, state = release_checkout(tmp_path, docker=STUB_DOCKER_IMAGES, scripts=(
        'build/run.sh', 'build/release-signing.pub', 'scripts/release-local.sh',
        'scripts/release-sign.sh', 'scripts/release-cosign.sh', 'scripts/release-assets.py',
        'scripts/release-publish.sh', 'scripts/release-verify.sh'))
    package = checkout / 'target/display-package'
    package.mkdir(parents=True)
    (package / 'PACKAGE.json').write_text('{}\n')
    out = tmp_path / 'out'
    env.pop('COSIGN', None)
    env.update(VMLAB_VDISPLAY_PACKAGE_DIR=str(package), VMLAB_RELEASE_COSIGN_KEY='stub private half',
               COSIGN_PASSWORD='stub password', **extra)
    result = subprocess.run([str(checkout / 'scripts/release-local.sh'), '--version', '9.9.9',
                             '--signing', 'key', '--builder', 'helios', '--out', str(out), *args],
                            capture_output=True, text=True, env=env)
    runs = state / 'runs'
    return result, [json.loads(line) for line in runs.read_text().splitlines()] if runs.exists() else [], out


def build_image_of(out):
    statement = json.loads((out / 'provenance.json').read_text())
    return statement['predicate']['buildDefinition']['internalParameters']['buildImage']


@pytest.mark.parametrize('retag', [False, True])
def test_a_release_builds_and_signs_in_the_one_image_of_its_provenance(tmp_path, retag):
    """Review install-release-2 F12: the tag `vmlab-build` is one name for the
    whole docker host, and a build of another checkout moves it. Every container
    of a release, the signing one included, names the image by the id that the
    release built, which is the id that provenance.json holds. A tag that moves
    while the release builds changes nothing: no container asks the tag again."""
    result, runs, out = signed_release(tmp_path, **({'STUB_RETAG': '1'} if retag else {}))
    assert result.returncode == 0, result.stderr
    # The build, the signature, its check, and the check of the upload command.
    assert len(runs) == 4, runs
    assert [run['image'] for run in runs] == [IMAGE_A] * 4, runs
    assert [run['secrets'] for run in runs] == [False, True, False, False]
    assert build_image_of(out) == IMAGE_A
    assert (out / 'SHA256SUMS.key.bundle').is_file()
    if retag:
        # The tag did move under the release: the signing container started
        # while the tag named the other image.
        assert runs[1]['tag'] == IMAGE_B


def test_a_real_version_does_not_take_the_image_tag_as_it_is(tmp_path):
    """Review install-release-2 F12: --image-as-is takes whatever image the tag
    names, built from any checkout. It is for the dry-run version only."""
    result, runs, out = signed_release(tmp_path, '--image-as-is')
    assert result.returncode != 0
    assert '--image-as-is is for the version 0.0.0-dry-run only' in result.stderr, result.stderr
    assert runs == [] and not out.exists()


def test_a_release_refuses_a_cache_in_the_checkout(tmp_path):
    """Review install-release-2 F13: `.ci-cache/` is ignored by the committed
    `.gitignore`, so the untracked-file check does not see it, and a build
    that ran earlier in the checkout can leave anything there."""
    checkout, env, state = release_checkout(tmp_path)
    (checkout / '.ci-cache/x').mkdir(parents=True)
    (checkout / '.ci-cache/x/planted').write_text('planted\n')

    release = release_dry_run(checkout, env)
    assert release.returncode != 0
    assert '\n  .ci-cache/x\n' in release.stderr, release.stderr
    # Refused before any container ran.
    assert not (state / 'runs').exists()
    # An empty cache directory is no refusal.
    (checkout / '.ci-cache/x/planted').unlink()
    (checkout / '.ci-cache/x').rmdir()
    release = release_dry_run(checkout, env)
    assert 'the build left no' in release.stderr, release.stderr
