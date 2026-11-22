#!/usr/bin/env bash
# Install the cosign that a release is signed and verified with, pinned by
# version and sha256, for the account that runs it. No root, no package
# manager: one static binary under ~/.local/bin (card 444;
# docs/agents/release-gate.md, K5).
#
# The version is pinned because signing with cosign v3 fails on
# `--tlog-upload=false` (scripts/release-sign.sh). The sha256 is the one of
# cosign_checksums.txt in the sigstore/cosign release v2.6.1.
#
# Usage: install-cosign.sh [dir]
#   dir  where the binary goes (default: ~/.local/bin)
#
# It makes no key and reads none.

set -euo pipefail

VERSION=v2.6.1
SHA256=064954c5d8c7e3b28188eee5b1727b31c411550bc5fefd41aa672d3c761d103a
URL="https://github.com/sigstore/cosign/releases/download/$VERSION/cosign-linux-amd64"

die() {
  echo "install-cosign: error: $*" >&2
  exit 1
}

[ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] || die "the pin is for Linux x86_64 only"
dir="${1:-$HOME/.local/bin}"
[ -n "$dir" ] && [ "$dir" != / ] || die "the directory is empty or /"
mkdir -p "$dir"
dir="$(cd "$dir" && pwd)"
[ -n "$dir" ] && [ "$dir" != / ] || die "the directory is empty or /"

if [ -x "$dir/cosign" ] && echo "$SHA256  $dir/cosign" | sha256sum --check --quiet 2>/dev/null; then
  echo "install-cosign: $dir/cosign is cosign $VERSION already" >&2
  exit 0
fi

# Download beside the target, so the rename is on one filesystem, and never
# leave an unchecked file under the final name.
tmp="$(mktemp "$dir/.cosign.XXXXXX")"
[ -n "$tmp" ] && [ "$tmp" != / ] && [ -f "$tmp" ] || die "mktemp gave no file"
trap 'rm -f "$tmp"' EXIT
curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp" "$URL" || die "could not download $URL"
echo "$SHA256  $tmp" | sha256sum --check --quiet \
  || die "the download does not have the pinned sha256 $SHA256"
chmod 0755 "$tmp"
mv -f "$tmp" "$dir/cosign"
trap - EXIT
"$dir/cosign" version >/dev/null 2>&1 || die "$dir/cosign does not run"
echo "install-cosign: installed cosign $VERSION at $dir/cosign" >&2
