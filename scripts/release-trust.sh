#!/usr/bin/env bash
# Turn a public key on as the trust root of the key-signed releases
# (`just release::trust <cosign.pub>`), or check that the switch is consistent.
#
# The public half of the release key is in two places, and they must hold the
# same PEM text:
#
#   build/release-signing.pub   what release-sign.sh, release-verify.sh and
#                               release-publish.sh verify a signature with
#   RELEASE_KEY='...' in install.sh   what an installer verifies with
#
# With a file, this script writes both from it and then checks them. With
# --check it writes nothing: it passes when both hold the same key, or when
# neither holds one (the switch is off: releases are unsigned and install.sh
# trusts no key), and fails on everything between. `just ci::release-dry-run`
# runs --check.
#
# It takes a public key only, and refuses a file that holds a private one.
# It commits nothing.
#
# Usage: release-trust.sh <cosign.pub>
#        release-trust.sh --check
# Env: VMLAB_RELEASE_TRUST_ROOT  the checkout to write and check (default:
#                                this one; the tests give a copy)

set -euo pipefail

ROOT="${VMLAB_RELEASE_TRUST_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

die() {
  echo "release-trust: error: $*" >&2
  exit 1
}

[ $# -eq 1 ] && [ -n "$1" ] || die "usage: release-trust.sh <cosign.pub> | --check"
[ -n "$ROOT" ] && [ "$ROOT" != / ] && [ -f "$ROOT/install.sh" ] || die "no vmlab checkout at '$ROOT'"
source_key=""
if [ "$1" != --check ]; then
  [ -f "$1" ] || die "no public key file at $1"
  source_key="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
fi

python3 - "$ROOT" "$source_key" <<'PY'
import hashlib
import os
import re
import sys
from pathlib import Path

root, source = Path(sys.argv[1]), sys.argv[2]
committed = root / 'build/release-signing.pub'
install = root / 'install.sh'
SWITCH = re.compile(r"^RELEASE_KEY='([^']*)'$", re.M)
PEM = re.compile(r'-----BEGIN PUBLIC KEY-----\n(?:[A-Za-z0-9+/=]{1,64}\n)+-----END PUBLIC KEY-----')


def die(message):
    sys.exit(f'release-trust: error: {message}')


def public_key(path):
    """The PEM text of a public key file, without the last newline."""
    text = path.read_text().replace('\r\n', '\n').strip()
    if 'PRIVATE' in text:
        die(f'{path} holds a private key. This takes cosign.pub only. '
            'Give the private half to nobody and to no file of the checkout')
    if not PEM.fullmatch(text):
        die(f'{path} is not one PEM public key (-----BEGIN PUBLIC KEY----- ... -----END PUBLIC KEY-----)')
    return text


def write(path, text):
    temporary = path.with_name(path.name + '.trust-tmp')
    temporary.write_text(text)
    if path.exists():
        os.chmod(temporary, path.stat().st_mode & 0o777)
    os.replace(temporary, path)


script = install.read_text()
switches = SWITCH.findall(script)
if len(switches) != 1:
    die(f"{install} must hold exactly one RELEASE_KEY='...' line, and has {len(switches)}")

if source:
    key = public_key(Path(source))
    write(committed, key + '\n')
    write(install, SWITCH.sub(lambda _: f"RELEASE_KEY='{key}'", script, count=1))
    script = install.read_text()

embedded = SWITCH.search(script)[1]
if not committed.exists():
    if embedded:
        die(f'{install} trusts a release key, and {committed} is missing: the two must hold the same key. '
            'Run `just release::trust <cosign.pub>`')
    print('release-trust: the switch is off: no build/release-signing.pub, and install.sh trusts no key. '
          'Releases are unsigned.', file=sys.stderr)
    sys.exit(0)
key = public_key(committed)
if embedded != key:
    what = 'trusts no key' if not embedded else 'holds another key'
    die(f'{committed} holds a public key, and RELEASE_KEY of {install} {what}: the two must hold the same key. '
        'Run `just release::trust <cosign.pub>`')
print(f'release-trust: build/release-signing.pub and RELEASE_KEY of install.sh hold the same public key '
      f"(sha256 of the PEM text: {hashlib.sha256((key + chr(10)).encode()).hexdigest()}).", file=sys.stderr)
if source:
    print('release-trust: commit build/release-signing.pub and install.sh, and deploy that install.sh '
          'to vmlab.io before the first key-signed release (docs/agents/release-gate.md).', file=sys.stderr)
PY
