#!/usr/bin/env bash
# Make the cosign key pair of the releases (`just release::keygen <dir>`).
# For the operator, at a terminal: cosign asks for the password of the private
# half twice, and nothing else ever holds it. An agent does not run this for
# the real key (docs/agents/release-gate.md, D2).
#
# cosign runs in the vmlab-build container (scripts/release-cosign.sh) with
# the terminal attached, no network, and <dir> as its only mount. It writes
#
#   <dir>/cosign.key  the private half, encrypted with the password (mode 600)
#   <dir>/cosign.pub  the public half (mode 644)
#
# <dir> must be outside every git checkout, so the private half cannot be
# committed, and must not hold a key already. Put it on a medium that is not
# the disk of the build machine: the build machine never needs cosign.key.
#
# Usage: release-keygen.sh <dir>
# Env: COSIGN, VMLAB_BUILD_IMAGE  see scripts/release-cosign.sh

set -euo pipefail
set +x

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() {
  echo "release-keygen: error: $*" >&2
  exit 1
}

[ $# -eq 1 ] && [ -n "$1" ] || die "usage: release-keygen.sh <dir>"
# Refused before anything is made: the path as it will be, links resolved.
dir="$(realpath -m -- "$1")"
[ -n "$dir" ] && [ "$dir" != / ] || die "the directory is empty or /"
root="$(cd "$ROOT" && pwd -P)"
case "$dir/" in
  "$root"/*) die "$dir is inside the checkout $root: a key is never made where git can add it" ;;
esac
umask 077
mkdir -p "$dir"
if command -v git >/dev/null 2>&1 && git -C "$dir" rev-parse --show-toplevel >/dev/null 2>&1; then
  die "$dir is inside the git checkout $(git -C "$dir" rev-parse --show-toplevel): a key is never made where git can add it"
fi
for name in cosign.key cosign.pub; do
  [ ! -e "$dir/$name" ] || die "$dir/$name exists already: this never overwrites a key"
done

echo "release-keygen: cosign asks for the password of the private half, twice." >&2
# An inherited password would make a key that nobody typed a password for.
env -u COSIGN_PASSWORD bash "$ROOT/scripts/release-cosign.sh" --rw "$dir" --workdir "$dir" --tty -- \
  generate-key-pair || die "cosign made no key pair"
[ -s "$dir/cosign.key" ] && [ -s "$dir/cosign.pub" ] || die "cosign left no cosign.key and cosign.pub in $dir"
chmod 600 "$dir/cosign.key"
chmod 644 "$dir/cosign.pub"
grep -q 'ENCRYPTED' "$dir/cosign.key" || die "$dir/cosign.key is not an encrypted key: remove it"

cat >&2 <<STEPS
release-keygen: made $dir/cosign.key (private, encrypted) and $dir/cosign.pub (public).

Next steps, for the operator:
  1. Store the content of cosign.key as the secret VMLAB_RELEASE_COSIGN_KEY of
     the factory, and the password as the secret COSIGN_PASSWORD.
  2. Keep a copy of cosign.key and of the password in a place of your own,
     off this machine. A lost key cannot be made again (release-gate.md, D4).
  3. Give cosign.pub to the repo: \`just release::trust $dir/cosign.pub\`,
     then commit build/release-signing.pub and install.sh.
  4. Remove $dir/cosign.key from this machine. No build step reads it from a
     file: signing takes it from the two variables.
STEPS
