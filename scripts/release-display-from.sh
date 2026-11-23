#!/usr/bin/env bash
# Stage the signed Windows display package of a published release, for
# `just release::local --display staged` (card 444;
# docs/agents/release-gate.md, "Windows display driver").
#
# The package of a release stays valid for a later commit as long as no build
# input of guest/vdisplay-windows changed: guest/vdisplay-windows/package.py
# compares the sha256 of every input that BUILDINFO.json of the package names
# with the file in the checkout, and refuses the package on a difference.
# Then the build lab must sign again (`--display rebuild`).
#
#   1. Download SHA256SUMS, its signature and the guest bundle of the release.
#   2. Verify the signature the way install.sh does: keyless against the
#      identity of ci.yml (SHA256SUMS.bundle, a release that GitHub built), or
#      with the committed release key (SHA256SUMS.key.bundle, a release built
#      on the build host). An unsigned release is refused.
#   3. Check the bundle against SHA256SUMS, and unpack vdisplay-windows/x86_64.
#   4. package.py of the target checkout verifies the package against that
#      checkout's sources and stages it in
#      <checkout>/guest/vdisplay-windows/target/signed-package.
#
# Usage: release-display-from.sh <tag> [checkout]
#   tag       the release that holds the package, for example v0.48.0-alpha
#   checkout  the checkout that gets the package (default: this one)
# Env: COSIGN    the cosign binary (default: cosign on PATH, v2.6.1 or newer:
#                scripts/install-cosign.sh)
#      BASE_URL  the release download URL (default: the GitHub releases of
#                VMLabDev/vmlab)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_URL="${BASE_URL:-https://github.com/VMLabDev/vmlab/releases/download}"
BASE_URL="${BASE_URL%/}"

die() {
  echo "release-display: error: $*" >&2
  exit 1
}

[ $# -ge 1 ] && [ $# -le 2 ] || die "usage: release-display-from.sh <tag> [checkout]"
tag="v${1#v}"
version="${tag#v}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$ ]] || die "invalid release tag: '$1'"
checkout="$(cd "${2:-$ROOT}" && pwd)"
[ -n "$checkout" ] && [ "$checkout" != / ] || die "the checkout is empty or /"
[ -f "$checkout/guest/vdisplay-windows/package.py" ] || die "no vmlab checkout at $checkout"
cosign="${COSIGN:-cosign}"
command -v "$cosign" >/dev/null 2>&1 \
  || die "need cosign to verify the release $tag (scripts/install-cosign.sh installs the pinned one)"

work="$(mktemp -d)"
[ -n "$work" ] && [ "$work" != / ] || die "unsafe work directory"
cleanup() {
  [ -n "$work" ] && [ "$work" != / ] || return 1
  rm -rf "$work"
}
trap cleanup EXIT

fetch() {
  curl -fsSL --proto '=https' --tlsv1.2 -o "$work/$1" "$BASE_URL/$tag/$1"
}

bundle="vmlab-guest-$version.tar.gz"
fetch SHA256SUMS || die "the release $tag has no SHA256SUMS"
if fetch SHA256SUMS.key.bundle 2>/dev/null; then
  key="$checkout/build/release-signing.pub"
  [ -s "$key" ] || die "the release $tag is signed with a release key, and $key is missing"
  "$cosign" verify-blob --key "$key" --bundle "$work/SHA256SUMS.key.bundle" \
    --insecure-ignore-tlog=true "$work/SHA256SUMS" >/dev/null 2>&1 \
    || die "the signature of the release $tag does not verify with $key"
elif fetch SHA256SUMS.bundle 2>/dev/null; then
  "$cosign" verify-blob --bundle "$work/SHA256SUMS.bundle" \
    --certificate-identity 'https://github.com/VMLabDev/vmlab/.github/workflows/ci.yml@refs/heads/main' \
    --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
    "$work/SHA256SUMS" >/dev/null 2>&1 \
    || die "the keyless signature of the release $tag does not verify"
else
  die "the release $tag has no signature: its display package is not taken"
fi
fetch "$bundle" || die "the release $tag has no $bundle"
expected="$(awk -v name="$bundle" '$2 == name || $2 == "*" name { print $1 }' "$work/SHA256SUMS")"
[ -n "$expected" ] || die "SHA256SUMS of $tag does not name $bundle"
echo "$expected  $work/$bundle" | sha256sum --check --quiet \
  || die "$bundle does not have the sha256 of SHA256SUMS"

mkdir "$work/bundle"
tar -xzf "$work/$bundle" -C "$work/bundle" --no-same-owner vdisplay-windows/x86_64 \
  || die "$bundle holds no vdisplay-windows/x86_64"
staged="$checkout/guest/vdisplay-windows/target/signed-package"
python3 "$checkout/guest/vdisplay-windows/package.py" "$work/bundle/vdisplay-windows/x86_64" "$work/staged" \
  || die "the display package of $tag does not fit the sources of $checkout: sign again with --display rebuild"
[ -n "$staged" ] && [ "$staged" != / ] || die "the staging directory is empty or /"
rm -rf "$staged"
mkdir -p "$(dirname "$staged")"
cp -r "$work/staged" "$staged"
echo "release-display: staged the display package of $tag (bundle sha256 $expected) in $staged" >&2
