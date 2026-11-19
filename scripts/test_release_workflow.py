"""Release branch dry builds must never acquire signing or publish rights."""
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = yaml.load((ROOT / '.github/workflows/ci.yml').read_text(), Loader=yaml.BaseLoader)
JOBS = WORKFLOW['jobs']


def test_release_pushes_build_and_prepare():
    assert 'release/**' in WORKFLOW['on']['push']['branches']
    assert JOBS['version']['if'] == "github.event_name == 'push'"
    calc = next(s['run'] for s in JOBS['version']['steps'] if s.get('id') == 'calc')
    assert 'do_release=false\\nversion=0.0.0-dry-run' in calc
    for name in ['build', 'vdisplay-build', 'guest-assets', 'hv-build', 'release-assets']:
        condition = JOBS[name]['if']
        assert "github.event_name == 'push'" in condition
        assert "startsWith(github.ref, 'refs/heads/release/')" in condition
    assert 'always() && !failure() && !cancelled()' in JOBS['guest-assets']['if']
    assert 'vdisplay-build' in JOBS['guest-assets']['needs']


def test_signing_and_publication_are_main_only():
    for name in ['vdisplay-sign', 'release']:
        condition = JOBS[name]['if']
        assert "github.ref == 'refs/heads/main'" in condition
        assert "github.event_name == 'push'" in condition
        assert "needs.version.outputs.do_release == 'true'" in condition
        assert '||' not in condition
    assert WORKFLOW['permissions'] == {'contents': 'read'}
    for name, job in JOBS.items():
        if name != 'release':
            assert 'write' not in job.get('permissions', {}).values()
    prepare = JOBS['release-assets']
    assert 'release-assets' in JOBS['release']['needs']
    commands = '\n'.join(step.get('run', '') for step in prepare['steps'])
    assert 'scripts/release-assets.py' in commands
    assert 'sha256sum --check SHA256SUMS' in commands
    assert 'cosign' not in commands


@pytest.mark.parametrize('matrix', JOBS['build']['strategy']['matrix']['include'])
def test_release_build_uploads_required_assets(tmp_path, matrix):
    version = '0.0.0-dry-run'
    expressions = {'needs.version.outputs.version': version,
                   'matrix.target': matrix['target'], 'matrix.suffix': matrix['suffix']}

    def render(value):
        return re.sub(r'\$\{\{\s*(.*?)\s*\}\}', lambda match: expressions[match[1]], value)

    # Stand in for the explicit Cargo binary outputs, then run the real staging step.
    steps = JOBS['build']['steps']
    build = next(step['run'] for step in steps if step.get('run', '').startswith('cargo build '))
    args = shlex.split(render(build))
    output = tmp_path / 'target' / args[args.index('--target') + 1] / 'release'
    output.mkdir(parents=True)
    for index, arg in enumerate(args):
        if arg == '--bin':
            (output / args[index + 1]).write_bytes(b'compiled fixture')
    stage = next(step['run'] for step in steps if step.get('name') == 'Stage assets')
    subprocess.run(['bash', '-e', '-c', render(stage)], cwd=tmp_path, check=True)

    # Stand in for the bundle recipes, then pass only the configured upload files
    # to the same checksum and provenance step used on release branches.
    for directory, name in [
        ('target/guest-package', f'vmlab-guest-{version}.tar.gz'),
        ('target/dist', f'vmlab-hv-{version}-linux-x86_64.tar.gz'),
    ]:
        output = tmp_path / directory
        output.mkdir(parents=True)
        (output / name).write_bytes(b'bundle fixture')
        (output / f'{name}.sha256').write_bytes(b'checksum fixture')

    dist = tmp_path / 'dist'
    dist.mkdir()
    for job in ('build', 'guest-assets', 'hv-build'):
        upload = next(step['with'] for step in JOBS[job]['steps']
                      if step.get('uses', '').startswith('actions/upload-artifact@'))
        for pattern in render(upload['path']).splitlines():
            for path in tmp_path.glob(pattern):
                shutil.copy2(path, dist)

    prepare = next(step for step in JOBS['release-assets']['steps']
                   if 'scripts/release-assets.py' in step.get('run', ''))
    (tmp_path / 'scripts').mkdir()
    shutil.copy2(ROOT / 'scripts/release-assets.py', tmp_path / 'scripts')
    env = dict(os.environ, VERSION=render(prepare['env']['VERSION']),
               GITHUB_SHA='a' * 40, GITHUB_RUN_ID='123')
    subprocess.run(['bash', '-e', '-c', prepare['run']], cwd=tmp_path, env=env, check=True)


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
