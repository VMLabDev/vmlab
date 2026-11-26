#!/usr/bin/env bash
# Say what this machine lacks to build and sign a release
# (`just release::doctor [key|none]`). It changes nothing and builds nothing.
#
# A build machine needs docker, a full checkout and the commands that the
# release scripts call on the host. Everything else is in the vmlab-build
# image, cosign included. Signing needs the two variables of the release key
# in the environment: this script says whether each is set, and never reads
# or prints a value.
#
# Each line is one check:
#   ok       there
#   MISSING  needed for the way asked for: the exit code is 1
#   note     not needed to build, or made by `just release::local` itself
#
# Usage: release-doctor.sh [key|none]
#   The signing way to check for. Default: $VMLAB_RELEASE_SIGNING, then none.
#   With `key`, an unset variable or a missing public key is MISSING.
# Env: VMLAB_BUILD_IMAGE (default vmlab-build)
#      VMLAB_RELEASE_MIN_FREE_GIB (default 15): the free space a cold build needs

set -euo pipefail
set +x

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${VMLAB_BUILD_IMAGE:-vmlab-build}"
MIN_FREE="${VMLAB_RELEASE_MIN_FREE_GIB:-15}"
way="${1:-${VMLAB_RELEASE_SIGNING:-none}}"
case "$way" in
  key|none) ;;
  *) echo "release-doctor: error: the way is key or none, not '$way'" >&2; exit 2 ;;
esac
[[ "$MIN_FREE" =~ ^[0-9]+$ ]] || { echo "release-doctor: error: VMLAB_RELEASE_MIN_FREE_GIB is not a number" >&2; exit 2; }

missing=0
ok() { printf 'ok       %s\n' "$*"; }
note() { printf 'note     %s\n' "$*"; }
miss() {
  printf 'MISSING  %s\n' "$*"
  missing=$((missing + 1))
}
# for_key <text>: MISSING when the way is key, a note otherwise.
for_key() {
  if [ "$way" = key ]; then miss "$*"; else note "$* (needed for --signing key only)"; fi
}

echo "release-doctor: $(hostname -s 2>/dev/null || uname -n), checkout $ROOT, signing way: $way"

# ── Host commands ────────────────────────────────────────────────────────────
for tool in git just python3 sha256sum od; do
  if command -v "$tool" >/dev/null 2>&1; then ok "$tool on PATH"; else miss "$tool on PATH: the release scripts call it on the host"; fi
done
if command -v gh >/dev/null 2>&1; then
  ok "gh on PATH (for \`just release::publish\` only)"
else
  note "no gh on PATH: needed for \`just release::publish\` only, not to build or sign"
fi
if command -v cosign >/dev/null 2>&1; then
  note "a cosign is on the PATH of the host: no release step uses it"
else
  ok "no cosign on the host, and none is needed: it runs in the image"
fi

# ── Docker and the image ─────────────────────────────────────────────────────
docker_ok=0
if ! command -v docker >/dev/null 2>&1; then
  miss "docker on PATH: every build and every cosign step runs in a container"
elif ! docker info >/dev/null 2>&1; then
  miss "docker is installed, and this account cannot reach the daemon (is it running, is the account in the docker group?)"
else
  docker_ok=1
  ok "docker $(docker version -f '{{.Server.Version}}' 2>/dev/null || echo '?'), daemon reachable"
fi
pin="$(sed -n 's/^VERSION=//p' "$ROOT/scripts/install-cosign.sh" | head -n 1)"
if [ "$docker_ok" -eq 1 ]; then
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    note "no image $IMAGE yet: \`just release::local\` builds it first (about 10 GiB, a long first build)"
    for_key "cosign: it comes with the image $IMAGE. Build it with \`just buildbox::image\`"
  else
    ok "image $IMAGE ($(docker image inspect -f '{{.Id}}' "$IMAGE" | cut -c1-19))"
    have="$(docker image inspect -f '{{index .Config.Labels "io.vmlab.cosign"}}' "$IMAGE" 2>/dev/null || true)"
    if [ -z "$have" ]; then
      for_key "cosign in the image $IMAGE: the image is from before cosign was added. Build it again with \`just buildbox::image\`"
    elif [ "$have" != "$pin" ]; then
      for_key "cosign $pin in the image $IMAGE: it has $have. Build it again with \`just buildbox::image\`"
    else
      ok "cosign $have in the image $IMAGE (the pin of scripts/install-cosign.sh)"
    fi
  fi
fi

# ── Disk space ───────────────────────────────────────────────────────────────
# free_gib <path>: whole GiB free for this account on the filesystem of <path>.
free_gib() { df -Pk -- "$1" 2>/dev/null | awk 'NR == 2 { printf "%d", $4 / 1048576 }'; }
space() {
  local what="$1" path="$2" free
  free="$(free_gib "$path" || true)"
  if [ -z "$free" ]; then
    note "could not read the free space of $what ($path)"
  elif [ "$free" -lt "$MIN_FREE" ]; then
    miss "disk space for $what ($path): $free GiB free, a cold build needs $MIN_FREE GiB"
  else
    ok "$free GiB free for $what ($path), $MIN_FREE GiB needed"
  fi
}
space "the checkout" "$ROOT"
if [ "$docker_ok" -eq 1 ]; then
  docker_root="$(docker info -f '{{.DockerRootDir}}' 2>/dev/null || true)"
  [ -z "$docker_root" ] || space "the docker volumes and the image" "$docker_root"
fi

# ── The checkout ─────────────────────────────────────────────────────────────
if command -v git >/dev/null 2>&1 && git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
  if [ "$(git -C "$ROOT" rev-parse --is-shallow-repository)" = true ]; then
    miss "a full checkout: this one is shallow, and the version comes from the history and the tags"
  else
    ok "full checkout at $(git -C "$ROOT" rev-parse --short HEAD)"
  fi
  if [ -n "$(git -C "$ROOT" tag --list 'v*' | head -n 1)" ]; then
    ok "the v* tags are fetched"
  else
    note "no v* tag in this checkout: fetch the tags, the version of a release is counted from the last one"
  fi
  if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
    note "tracked files have uncommitted changes: \`just release::local\` refuses that"
  fi
else
  miss "a git checkout at $ROOT: the provenance names its HEAD"
fi
package="${VMLAB_VDISPLAY_PACKAGE_DIR:-$ROOT/guest/vdisplay-windows/target/signed-package}"
if [ -s "$package/PACKAGE.json" ]; then
  ok "a signed Windows display package is staged ($package)"
else
  note "no signed Windows display package staged: \`scripts/release-display-from.sh <tag>\` stages the one of a release, or use --display rebuild"
fi

# ── The release key: names and states only, never a value ───────────────────
for name in VMLAB_RELEASE_COSIGN_KEY COSIGN_PASSWORD; do
  if [ -n "${!name:+set}" ]; then
    ok "$name is set"
  else
    for_key "$name is not set"
  fi
done
if [ -s "$ROOT/build/release-signing.pub" ]; then
  if bash "$ROOT/scripts/release-trust.sh" --check >/dev/null 2>&1; then
    ok "build/release-signing.pub is committed, and install.sh trusts the same key"
  else
    miss "build/release-signing.pub and RELEASE_KEY of install.sh differ: \`just release::trust <cosign.pub>\`"
  fi
elif bash "$ROOT/scripts/release-trust.sh" --check >/dev/null 2>&1; then
  for_key "no public key in the repo yet (build/release-signing.pub): the operator gives one with \`just release::trust <cosign.pub>\`"
else
  miss "install.sh trusts a key, and build/release-signing.pub is missing: \`just release::trust <cosign.pub>\`"
fi

if [ "$missing" -eq 0 ]; then
  if [ "$way" = key ]; then
    echo "release-doctor: this machine can build and sign a release: just release::local ... --signing key"
  else
    echo "release-doctor: this machine can build an unsigned release: just release::local ... (run \`just release::doctor key\` to check for signing)"
  fi
  exit 0
fi
echo "release-doctor: $missing missing for a release with the signing way '$way'"
exit 1
