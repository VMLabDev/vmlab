#!/usr/bin/env python3
"""Create the release checksum manifest and SLSA provenance before signing."""
import hashlib
import json
import os
from pathlib import Path
import re
import sys


def prepare(directory, version):
    root = Path(directory).resolve()
    if root == Path('/') or not root.is_dir():
        raise ValueError('release directory must exist and must not be /')
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?', version):
        raise ValueError('invalid release version')
    names = [f'vmlab-{version}-linux-x86_64',
             f'vmlab-guest-{version}.tar.gz',
             f'vmlab-hv-{version}-linux-x86_64.tar.gz']
    subjects = []
    for name in names:
        path = root / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f'missing regular release asset: {name}')
        subjects.append({'name': name, 'digest': {'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}})
    repo = 'https://github.com/VMLabDev/vmlab'
    statement = {
        '_type': 'https://in-toto.io/Statement/v1',
        'subject': subjects,
        'predicateType': 'https://slsa.dev/provenance/v1',
        'predicate': {
            'buildDefinition': {
                'buildType': repo + '/.github/workflows/ci.yml',
                'externalParameters': {'version': version},
                'internalParameters': {},
                'resolvedDependencies': [{'uri': repo, 'digest': {'gitCommit': os.environ['GITHUB_SHA']}}],
            },
            'runDetails': {
                'builder': {'id': repo + '/.github/workflows/ci.yml@refs/heads/main'},
                'metadata': {'invocationId': repo + '/actions/runs/' + os.environ['GITHUB_RUN_ID']},
            },
        },
    }
    provenance = root / 'provenance.json'
    provenance.write_text(json.dumps(statement, sort_keys=True, indent=2) + '\n')
    entries = [(s['name'], s['digest']['sha256']) for s in subjects]
    entries.append((provenance.name, hashlib.sha256(provenance.read_bytes()).hexdigest()))
    (root / 'SHA256SUMS').write_text(''.join(f'{digest}  {name}\n' for name, digest in sorted(entries)))


if __name__ == '__main__':
    prepare(*sys.argv[1:])
