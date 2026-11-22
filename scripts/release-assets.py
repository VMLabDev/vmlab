#!/usr/bin/env python3
"""The checksum manifest and SLSA provenance of a locally built release.

  release-assets.py prepare DIR VERSION --commit SHA --builder HOST [--image ID]
      Refuse a directory that lacks one of the four release assets, then
      write provenance.json and SHA256SUMS into it. Signing comes after
      (scripts/release-sign.sh).
  release-assets.py verify DIR
      Check that every asset is there and that SHA256SUMS and provenance.json
      still describe the files. Prints version=, commit=, builder= and
      prerelease= lines.
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import re
import sys

REPO = 'https://github.com/VMLabDev/vmlab'
BUILD_TYPE = REPO + '/scripts/release-local.sh'
VERSION = r'[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?'


def asset_names(version):
    return [f'vmlab-{version}-linux-x86_64',
            f'vmlab-fastpath-{version}-linux-x86_64',
            f'vmlab-guest-{version}.tar.gz',
            f'vmlab-hv-{version}-linux-x86_64.tar.gz']


def release_dir(directory):
    root = Path(directory).resolve()
    if root == Path('/') or not root.is_dir():
        raise ValueError('release directory must exist and must not be /')
    return root


def digest(path):
    with path.open('rb') as file:
        return hashlib.file_digest(file, 'sha256').hexdigest()


def subjects(root, version):
    found = []
    for name in asset_names(version):
        path = root / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f'missing regular release asset: {name}')
        if path.stat().st_size == 0:
            raise ValueError(f'empty release asset: {name}')
        found.append({'name': name, 'digest': {'sha256': digest(path)}})
    return found


def prepare(directory, version, commit, builder, image=None, started=None):
    root = release_dir(directory)
    if not re.fullmatch(VERSION, version):
        raise ValueError('invalid release version')
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('the commit must be a full 40-digit hash')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*', builder):
        raise ValueError('invalid builder host name')
    started = started or datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    statement = {
        '_type': 'https://in-toto.io/Statement/v1',
        'subject': subjects(root, version),
        'predicateType': 'https://slsa.dev/provenance/v1',
        'predicate': {
            'buildDefinition': {
                'buildType': BUILD_TYPE,
                'externalParameters': {'version': version},
                'internalParameters': {'buildImage': image} if image else {},
                'resolvedDependencies': [{'uri': REPO, 'digest': {'gitCommit': commit}}],
            },
            'runDetails': {
                # Not a GitHub workflow: the vmlab-build container on a named host.
                'builder': {'id': f'{BUILD_TYPE}@{commit}#host={builder}'},
                'metadata': {'invocationId': f'{builder}/{started}', 'startedOn': started},
            },
        },
    }
    provenance = root / 'provenance.json'
    provenance.write_text(json.dumps(statement, sort_keys=True, indent=2) + '\n')
    entries = [(s['name'], s['digest']['sha256']) for s in statement['subject']]
    entries.append((provenance.name, digest(provenance)))
    (root / 'SHA256SUMS').write_text(''.join(f'{value}  {name}\n' for name, value in sorted(entries)))


def verify(directory):
    """Every asset is there, and the manifest and provenance describe these files."""
    root = release_dir(directory)
    try:
        statement = json.loads((root / 'provenance.json').read_text())
        predicate = statement['predicate']
        version = predicate['buildDefinition']['externalParameters']['version']
        commit = predicate['buildDefinition']['resolvedDependencies'][0]['digest']['gitCommit']
        builder = predicate['runDetails']['builder']['id']
    except (OSError, ValueError, KeyError, IndexError, TypeError) as error:
        raise ValueError(f'provenance.json is missing or not a release provenance: {error}') from None
    if not isinstance(version, str) or not re.fullmatch(VERSION, version):
        raise ValueError('invalid release version in provenance.json')
    if not isinstance(commit, str) or not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('invalid commit in provenance.json')
    if statement.get('subject') != subjects(root, version):
        raise ValueError('provenance.json does not describe the assets in this directory')
    expected = {s['name']: s['digest']['sha256'] for s in statement['subject']}
    expected['provenance.json'] = digest(root / 'provenance.json')
    try:
        lines = (root / 'SHA256SUMS').read_text().splitlines()
    except OSError:
        raise ValueError('missing SHA256SUMS') from None
    manifest = {}
    for line in lines:
        fields = line.split()
        if len(fields) != 2 or fields[1] in manifest:
            raise ValueError(f'bad SHA256SUMS line: {line}')
        manifest[fields[1]] = fields[0]
    if manifest != expected:
        raise ValueError('SHA256SUMS does not match the assets in this directory')
    return {'version': version, 'commit': commit, 'builder': builder,
            'prerelease': 'true' if '-' in version else 'false'}


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)
    make = commands.add_parser('prepare')
    make.add_argument('directory')
    make.add_argument('version')
    make.add_argument('--commit', required=True)
    make.add_argument('--builder', required=True)
    make.add_argument('--image')
    check = commands.add_parser('verify')
    check.add_argument('directory')
    arguments = parser.parse_args(argv)
    try:
        if arguments.command == 'prepare':
            prepare(arguments.directory, arguments.version, arguments.commit, arguments.builder,
                    arguments.image)
        else:
            for key, value in verify(arguments.directory).items():
                print(f'{key}={value}')
    except ValueError as error:
        print(f'release-assets: error: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
