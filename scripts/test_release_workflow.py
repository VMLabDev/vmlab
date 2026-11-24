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


@pytest.mark.parametrize('version,prerelease', [('1.2.3-alpha', True), ('1.2.3', False)])
def test_publish_prints_the_one_upload_command(tmp_path, version, prerelease):
    dist = release_dir(tmp_path, version)
    (dist / 'SHA256SUMS.key.bundle').write_text('signature fixture')
    result = publish(dist, '--print')
    assert result.returncode == 0, result.stderr
    words = result.stdout.split()
    assert words[:9] == ['gh', 'release', 'create', f'v{version}', '--repo', 'VMLabDev/vmlab',
                         '--target', COMMIT, '--title']
    assert ('--prerelease' in words) == prerelease
    uploaded = sorted(Path(word).name for word in words if word.startswith(str(dist)))
    assert uploaded == sorted(path.name for path in dist.iterdir())
    assert 'UNSIGNED' not in result.stderr


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


def release_checkout(tmp_path):
    """A committed checkout with the release scripts and a `.gitignore`, and the
    environment in which `docker` is the stub. Returns (checkout, env, state)."""
    checkout = tmp_path / 'checkout'
    for name in ['build/run.sh', 'scripts/release-local.sh']:
        (checkout / name).parent.mkdir(parents=True, exist_ok=True)
        (checkout / name).write_bytes((ROOT / name).read_bytes())
        (checkout / name).chmod(0o755)
    (checkout / '.gitignore').write_text('/target\n')
    git = ['git', '-C', str(checkout), '-c', 'user.name=t', '-c', 'user.email=t@example.invalid']
    subprocess.run([*git, 'init', '-q'], check=True)
    subprocess.run([*git, 'add', '-A'], check=True)
    subprocess.run([*git, 'commit', '-q', '-m', 'release commit'], check=True)

    bin_dir = tmp_path / 'bin'
    bin_dir.mkdir()
    (bin_dir / 'docker').write_text(STUB_DOCKER)
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
