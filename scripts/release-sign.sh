#!/usr/bin/env bash
# The one signing step of a locally built release (cards 423 and 1021). It
# signs the SHA256SUMS of a release directory in one of two ways:
#
#   key    A cosign key pair. The private half comes from the environment
#          variable VMLAB_RELEASE_COSIGN_KEY (the content of the encrypted
#          cosign.key, never a file on the build machine) and its password
#          from COSIGN_PASSWORD. The public half is build/release-signing.pub.
#          Writes SHA256SUMS.key.bundle, then verifies it with the public
#          half, the way install.sh does. Nothing goes to a transparency log.
#   none   No signature. The release holds SHA256SUMS only, and install.sh
#          refuses it without --insecure-skip-signature.
#
# cosign runs in the vmlab-build container (scripts/release-cosign.sh). The
# two variables reach it through the environment of `docker run` and are in
# no argument list, no file and no log: this script never prints them, and
# turns shell tracing off first. A variable that is not set is an error that
# names the variable.
#
# The keyless signature of ci.yml (the GitHub OIDC identity of the workflow)
# cannot be made outside a GitHub runner, so it is not a way here.
#
# Usage: release-sign.sh <release-dir> [key|none]
#   The way defaults to $VMLAB_RELEASE_SIGNING, and then to none.
# Env: VMLAB_RELEASE_COSIGN_KEY, COSIGN_PASSWORD (key)
#      VMLAB_RELEASE_PUBLIC_KEY  another public half than the committed one
#                                (a test key: never for a published release)
#      COSIGN, VMLAB_BUILD_IMAGE see scripts/release-cosign.sh

set -euo pipefail
set +x

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE=SHA256SUMS.key.bundle
COSIGN_RUN="$ROOT/scripts/release-cosign.sh"

die() {
  echo "release-sign: error: $*" >&2
  exit 1
}

# sign_release <dir> <way>: the single place a release signature is made.
sign_release() {
  local dir="$1" way="$2" public
  [ -n "$dir" ] && [ "$dir" != / ] && [ -d "$dir" ] || die "the release directory is empty, / or missing"
  [ -s "$dir/SHA256SUMS" ] || die "no SHA256SUMS in $dir: run scripts/release-assets.py prepare first"
  # A signature of an earlier manifest must never stay beside a new one.
  rm -f "$dir/$BUNDLE" "$dir/SHA256SUMS.bundle"
  case "$way" in
    none)
      echo "release-sign: UNSIGNED release: SHA256SUMS only. install.sh needs --insecure-skip-signature for it." >&2
      ;;
    key)
      dir="$(cd "$dir" && pwd)"
      [ -n "$dir" ] && [ "$dir" != / ] || die "the release directory is empty or /"
      # Names only: the value of a variable never reaches a message.
      [ -n "${VMLAB_RELEASE_COSIGN_KEY:+set}" ] \
        || die "VMLAB_RELEASE_COSIGN_KEY is not set: it holds the private half of the release key (the content of cosign.key)"
      [ -n "${COSIGN_PASSWORD:+set}" ] \
        || die "COSIGN_PASSWORD is not set: it holds the password of the release key"
      public="${VMLAB_RELEASE_PUBLIC_KEY:-$ROOT/build/release-signing.pub}"
      [ -s "$public" ] || die "no public half at $public: see docs/agents/release-gate.md"
      public="$(cd "$(dirname "$public")" && pwd)/$(basename "$public")"
      bash "$COSIGN_RUN" --rw "$dir" --secrets -- \
        sign-blob --yes --key env://VMLAB_RELEASE_COSIGN_KEY --tlog-upload=false \
        --bundle "$dir/$BUNDLE" "$dir/SHA256SUMS" >/dev/null \
        || die "cosign could not sign SHA256SUMS"
      # Also proves that the secret and the committed public half are one pair.
      if ! bash "$COSIGN_RUN" --ro "$dir" --ro "$public" -- \
        verify-blob --key "$public" --bundle "$dir/$BUNDLE" \
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
