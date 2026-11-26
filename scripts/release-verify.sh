#!/usr/bin/env bash
# Check a release directory (`just release::verify <dir>`): every asset,
# SHA256SUMS and provenance.json, and the signature when there is one.
#
# SHA256SUMS.key.bundle is verified the way install.sh does it, with cosign in
# the vmlab-build container (scripts/release-cosign.sh) and the committed
# public half build/release-signing.pub. A directory without a signature
# passes as an unsigned release, and this script says so.
#
# Usage: release-verify.sh <release-dir>
# Env: VMLAB_RELEASE_PUBLIC_KEY  another public half than the committed one
#                                (a test key: `just release::publish` takes
#                                the committed one only)
#      COSIGN, VMLAB_BUILD_IMAGE see scripts/release-cosign.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() {
  echo "release-verify: error: $*" >&2
  exit 1
}

[ $# -eq 1 ] && [ -n "$1" ] && [ -d "$1" ] || die "usage: release-verify.sh <release-dir>"
dir="$(cd "$1" && pwd)"
[ -n "$dir" ] && [ "$dir" != / ] || die "the release directory is empty or /"

python3 "$ROOT/scripts/release-assets.py" verify "$dir" || die "the release directory failed its check"
(cd "$dir" && sha256sum --check SHA256SUMS) || die "a file does not have the sha256 of SHA256SUMS"

bundle="$dir/SHA256SUMS.key.bundle"
if [ ! -e "$bundle" ]; then
  echo "release-verify: UNSIGNED release: no SHA256SUMS.key.bundle. install.sh needs --insecure-skip-signature for it." >&2
  exit 0
fi
[ -s "$bundle" ] || die "$bundle is empty: sign again, or remove it for an unsigned release"
public="${VMLAB_RELEASE_PUBLIC_KEY:-$ROOT/build/release-signing.pub}"
[ -s "$public" ] || die "no public half at $public to check $bundle with: see docs/agents/release-gate.md"
public="$(cd "$(dirname "$public")" && pwd)/$(basename "$public")"
bash "$ROOT/scripts/release-cosign.sh" --check || die "cosign is needed to check $bundle"
bash "$ROOT/scripts/release-cosign.sh" --ro "$dir" --ro "$public" -- \
  verify-blob --key "$public" --bundle "$bundle" \
  --insecure-ignore-tlog=true "$dir/SHA256SUMS" >/dev/null 2>&1 \
  || die "$bundle does not verify with $public and this SHA256SUMS"
echo "release-verify: SHA256SUMS.key.bundle verifies with $public" >&2
