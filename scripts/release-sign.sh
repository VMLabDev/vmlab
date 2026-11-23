#!/usr/bin/env bash
# The one signing step of a locally built release (card 423). It signs the
# SHA256SUMS of a release directory in one of two ways:
#
#   key    A cosign key pair. The private half comes from the environment
#          variable VMLAB_RELEASE_COSIGN_KEY (the contents of the encrypted
#          cosign.key, never a file in the checkout) and its password from
#          COSIGN_PASSWORD. The public half is build/release-signing.pub.
#          Writes SHA256SUMS.key.bundle, then verifies it with the public
#          half, the way install.sh does. Nothing goes to a transparency log.
#   none   No signature. The release holds SHA256SUMS only, and install.sh
#          refuses it without --insecure-skip-signature.
#
# The keyless signature of ci.yml (the GitHub OIDC identity of the workflow)
# cannot be made outside a GitHub runner, so it is not a way here.
#
# Usage: release-sign.sh <release-dir> [key|none]
#   The way defaults to $VMLAB_RELEASE_SIGNING, and then to none.
# Env: VMLAB_RELEASE_COSIGN_KEY, COSIGN_PASSWORD (key)
#      VMLAB_RELEASE_PUBLIC_KEY  another public half than the committed one
#                                (a test key: never for a published release)
#      COSIGN                    the cosign binary (default: cosign on PATH)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE=SHA256SUMS.key.bundle

die() {
  echo "release-sign: error: $*" >&2
  exit 1
}

# sign_release <dir> <way>: the single place a release signature is made.
sign_release() {
  local dir="$1" way="$2" cosign public
  [ -n "$dir" ] && [ "$dir" != / ] && [ -d "$dir" ] || die "the release directory is empty, / or missing"
  [ -s "$dir/SHA256SUMS" ] || die "no SHA256SUMS in $dir: run scripts/release-assets.py prepare first"
  # A signature of an earlier manifest must never stay beside a new one.
  rm -f "$dir/$BUNDLE" "$dir/SHA256SUMS.bundle"
  case "$way" in
    none)
      echo "release-sign: UNSIGNED release: SHA256SUMS only. install.sh needs --insecure-skip-signature for it." >&2
      ;;
    key)
      cosign="${COSIGN:-cosign}"
      command -v "$cosign" >/dev/null 2>&1 || die "need cosign (v2.6.1) to sign with a key"
      [ -n "${VMLAB_RELEASE_COSIGN_KEY:-}" ] \
        || die "VMLAB_RELEASE_COSIGN_KEY is empty: it holds the private half of the release key"
      public="${VMLAB_RELEASE_PUBLIC_KEY:-$ROOT/build/release-signing.pub}"
      [ -s "$public" ] || die "no public half at $public: see docs/agents/release-gate.md"
      export COSIGN_PASSWORD="${COSIGN_PASSWORD:-}"
      "$cosign" sign-blob --yes --key env://VMLAB_RELEASE_COSIGN_KEY --tlog-upload=false \
        --bundle "$dir/$BUNDLE" "$dir/SHA256SUMS" >/dev/null \
        || die "cosign could not sign SHA256SUMS"
      # Also proves that the secret and the committed public half are one pair.
      if ! "$cosign" verify-blob --key "$public" --bundle "$dir/$BUNDLE" \
        --insecure-ignore-tlog=true "$dir/SHA256SUMS" >/dev/null 2>&1; then
        rm -f "$dir/$BUNDLE"
        die "the signature does not verify with $public: the private and public halves differ"
      fi
      echo "release-sign: signed SHA256SUMS with the release key: $dir/$BUNDLE" >&2
      ;;
    *) die "the way is key or none, not '$way'" ;;
  esac
}

[ $# -ge 1 ] && [ $# -le 2 ] || die "usage: release-sign.sh <release-dir> [key|none]"
sign_release "$1" "${2:-${VMLAB_RELEASE_SIGNING:-none}}"
