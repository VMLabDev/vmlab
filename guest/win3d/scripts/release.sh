#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Build guest release assets; --dry-run packages existing signed output only.
# --publish explicitly tags and publishes after building. Never combine it with --dry-run.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
ROOT=$(git -C "$REPO" rev-parse --show-toplevel)
cd "$REPO"
DRY_RUN=0
PUBLISH=0
VERSION=
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        --publish) PUBLISH=1 ;;
        -h|--help) echo "usage: $0 [version] [--dry-run | --publish]"; exit 0 ;;
        -*) echo "unknown option: $arg" >&2; exit 2 ;;
        *) [ -z "$VERSION" ] || exit 2; VERSION=$arg ;;
    esac
done
[ "$DRY_RUN:$PUBLISH" != 1:1 ] || { echo "--dry-run cannot publish" >&2; exit 2; }
PKG_VERSION=$(cat installer/VERSION)
VERSION=${VERSION:-$PKG_VERSION}
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || exit 2
[ "${VERSION%%-*}" = "$PKG_VERSION" ] || { echo "version does not match installer/VERSION" >&2; exit 2; }
dirty=$(git -C "$ROOT" status --porcelain --untracked-files=all -- guest/win3d)
[ -z "$dirty" ] || { echo "guest/win3d is dirty:" >&2; echo "$dirty" >&2; exit 1; }
COMMIT=$(git -C "$ROOT" log -1 --format=%H -- guest/win3d)
EPOCH=$(git -C "$ROOT" log -1 --format=%ct -- guest/win3d)
TAG=win3d-v$VERSION
SIGNING=${WIN3D_SIGNING_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/vmlab/signing/win3d}
export WIN3D_SIGNING_DIR=$SIGNING
for f in test-signing.key test-signing.crt test-signing.cer SIGN_TIME CERT-INFO.txt; do
    [ -f "$SIGNING/$f" ] || { echo "missing signing file: $SIGNING/$f" >&2; exit 1; }
done
echo "tag: $TAG"
echo "guest commit: $COMMIT"
echo "signing directory: $SIGNING"
if [ "$DRY_RUN" = 0 ]; then scripts/build.sh; fi
BUNDLE=vmlab-win3d-$PKG_VERSION
PACKAGE=${WIN3D_PACKAGE_DIR:-$REPO/out/package/$BUNDLE}
OUT=$REPO/out/release/$VERSION
mkdir -p "$OUT"
STAGE=$(mktemp -d "$OUT/.stage.XXXXXXXX")
trap 'rm -rf "$STAGE"' EXIT
python3 scripts/release/assemble.py guest "$PACKAGE" "$STAGE/$BUNDLE" "$PKG_VERSION" "$COMMIT" "$SIGNING/test-signing.cer"
CERT_SUBJECT=$(sed -n 's/^subject: *//p' "$SIGNING/CERT-INFO.txt")
CERT_SHA1=$(sed -n 's/^sha1 thumbprint: *//p' "$SIGNING/CERT-INFO.txt")
python3 scripts/release/assemble.py fill scripts/release/windows-README.txt "$STAGE/README.txt" --crlf \
    VERSION="$VERSION" COMMIT="$COMMIT" BUNDLE="$BUNDLE" CERT_SUBJECT="$CERT_SUBJECT" CERT_SHA1="$CERT_SHA1"
ZIP=$OUT/vmlab-win3d-$VERSION-windows-x64.zip
python3 scripts/release/assemble.py zip "$STAGE" "$ZIP" "$EPOCH"
python3 scripts/release/assemble.py notes "$OUT/RELEASE-NOTES.md" "$VERSION" "$COMMIT" "$EPOCH" "$SIGNING/CERT-INFO.txt" "$ZIP"
(cd "$OUT" && sha256sum "$(basename "$ZIP")" RELEASE-NOTES.md >SHA256SUMS)
if [ "$PUBLISH" = 1 ]; then
    git -C "$ROOT" tag -a "$TAG" "$COMMIT" -m "win3d $VERSION"
    git -C "$ROOT" push https://github.com/VMLabDev/vmlab.git "refs/tags/$TAG"
    flags=()
    [[ $VERSION != *-* ]] || flags+=(--prerelease)
    gh release create "$TAG" --repo VMLabDev/vmlab --verify-tag --title "win3d $VERSION" \
        --notes-file "$OUT/RELEASE-NOTES.md" "${flags[@]}" "$ZIP" "$OUT/SHA256SUMS"
fi
echo "release assets: $OUT (dry-run=$DRY_RUN, published=$PUBLISH)"
cat "$OUT/SHA256SUMS"
