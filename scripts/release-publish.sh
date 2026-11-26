#!/usr/bin/env bash
# Upload a release directory that scripts/release-local.sh built. This is the
# only step that touches GitHub, and it only uploads: it builds nothing, signs
# nothing, and changes no file (card 423; docs/agents/release-gate.md).
#
# It checks the directory again (every asset, SHA256SUMS, provenance.json, and
# SHA256SUMS.key.bundle against build/release-signing.pub when there is one),
# takes the version and the commit from provenance.json, and runs
#
#   gh release create v<version> --repo VMLabDev/vmlab --target <commit> \
#     --title v<version> --generate-notes [--prerelease] <files>
#
# A version with a suffix (0.49.0-alpha) is a pre-release. The tag is made by
# GitHub at <commit>, so that commit must be on GitHub already.
#
# Usage: release-publish.sh <release-dir> [--print]
#   --print  print the command and upload nothing
# Env: VMLAB_RELEASE_REPO (default VMLabDev/vmlab), and the GitHub login of gh.
#      COSIGN, VMLAB_BUILD_IMAGE  cosign runs in the vmlab-build container:
#      scripts/release-cosign.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() {
  echo "release-publish: error: $*" >&2
  exit 1
}

dir="" print=0
for arg in "$@"; do
  case "$arg" in
    --print) print=1 ;;
    -*) die "unknown option: $arg" ;;
    *) [ -z "$dir" ] || die "give one release directory"; dir="$arg" ;;
  esac
done
[ -n "$dir" ] && [ -d "$dir" ] || die "usage: release-publish.sh <release-dir> [--print]"
dir="$(cd "$dir" && pwd)"

info="$(python3 "$ROOT/scripts/release-assets.py" verify "$dir")" || die "the release directory failed its check"
version="$(printf '%s\n' "$info" | sed -n 's/^version=//p')"
commit="$(printf '%s\n' "$info" | sed -n 's/^commit=//p')"
prerelease="$(printf '%s\n' "$info" | sed -n 's/^prerelease=//p')"
[ "$version" != 0.0.0-dry-run ] || die "$version is a dry run, never a release"

files=("$dir/vmlab-$version-linux-x86_64" "$dir/vmlab-fastpath-$version-linux-x86_64"
       "$dir/vmlab-guest-$version.tar.gz" "$dir/vmlab-guest-$version.tar.gz.sha256"
       "$dir/vmlab-hv-$version-linux-x86_64.tar.gz" "$dir/vmlab-hv-$version-linux-x86_64.tar.gz.sha256"
       "$dir/SHA256SUMS" "$dir/provenance.json")
for file in "${files[@]}"; do
  [ -s "$file" ] || die "missing release file: $file"
done
bundle="$dir/SHA256SUMS.key.bundle"
if [ -e "$bundle" ]; then
  # Check the signature the way install.sh does, and only with the committed
  # public half: release-sign.sh also takes a test key, and a later `prepare`
  # leaves the signature of an earlier SHA256SUMS (review install-release F8).
  [ -s "$bundle" ] || die "$bundle is empty: sign again, or remove it for an unsigned release"
  public="$ROOT/build/release-signing.pub"
  [ -s "$public" ] || die "no public half at $public to check $bundle with: see docs/agents/release-gate.md"
  # cosign runs in the build container. A cosign that cannot run is told
  # apart from a signature that does not verify.
  bash "$ROOT/scripts/release-cosign.sh" --check || die "cosign is needed to check $bundle"
  bash "$ROOT/scripts/release-cosign.sh" --ro "$dir" --ro "$public" -- \
    verify-blob --key "$public" --bundle "$bundle" \
    --insecure-ignore-tlog=true "$dir/SHA256SUMS" >/dev/null 2>&1 \
    || die "$bundle does not verify with $public and this SHA256SUMS: sign again with the release key (just release::sign <dir> key)"
  files+=("$bundle")
else
  echo "release-publish: UNSIGNED release: install.sh will need --insecure-skip-signature" >&2
fi

command=(gh release create "v$version" --repo "${VMLAB_RELEASE_REPO:-VMLabDev/vmlab}"
         --target "$commit" --title "v$version" --generate-notes)
[ "$prerelease" != true ] || command+=(--prerelease)
command+=("${files[@]}")

if [ "$print" -eq 1 ]; then
  printf '%q ' "${command[@]}"
  printf '\n'
  exit 0
fi
exec "${command[@]}"
