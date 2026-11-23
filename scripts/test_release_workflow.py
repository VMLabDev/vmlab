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
