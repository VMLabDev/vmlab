#!/usr/bin/env bash
# Build one version of vmlab on this host, in the vmlab-build container, into
# a release directory. Nothing is uploaded: scripts/release-publish.sh does
# that, as a second command (card 423; docs/agents/release-gate.md).
#
#   1. The version: --version X, or scripts/release-version.sh with the kind
#      (--pre-release | --release) and the bump from the command line.
#   2. The Windows display driver package (--display, below).
#   3. `build/run.sh build/dist.sh <version>`: the CLI, the fast-path helper,
#      the strict guest bundle, the hypervisor bundle and the BPF objects.
#      Every container of a release gets docker volumes of this run alone (the
#      cargo home and the target dirs), new and empty, and removed at the end:
#      an earlier build on this host cannot leave crate sources or build
#      outputs for a release to compile in.
#   4. The four assets and the two bundle .sha256 files go to the release
#      directory. scripts/release-assets.py writes provenance.json (the
#      builder id names this host and the commit) and SHA256SUMS.
#   5. scripts/release-sign.sh signs SHA256SUMS: key or none.
#   6. The check: every asset is there, SHA256SUMS and provenance.json match
#      the files, and the CLI reports the version.
#
# Usage: release-local.sh (--pre-release | --release | --version X) [options]
#   --bump major|minor|patch  the bump for release-version.sh (default: from
#                             the commit subjects since the last v* tag)
#   --out DIR                 the release directory
#                             (default: target/release-local/v<version>)
#   --signing key|none        default: $VMLAB_RELEASE_SIGNING, then none
#   --builder NAME            the host name in the provenance
#                             (default: $VMLAB_RELEASE_BUILDER, then hostname)
#   --image-as-is             do not rebuild the vmlab-build image first: use the
#                             tag as it is (a test build on a host that has
#                             the image; a real version rebuilds it)
#   --display staged|rebuild|unsigned
#       staged    (default) use the signed package that is staged already, in
#                 $VMLAB_VDISPLAY_PACKAGE_DIR or
#                 guest/vdisplay-windows/target/signed-package
#       rebuild   run guest/vdisplay-windows/rebuild.py --route container
#                 first: compile in the container, catalog and test-sign in
#                 the Windows 11 build lab on this host
#       unsigned  compile only, in the container. For the version
#                 0.0.0-dry-run alone: the bundle has no catalog and no
#                 certificate, and release-publish.sh refuses that version.
#
# The checkout must have no uncommitted change to a tracked file: the
# provenance names HEAD as the source of every asset.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
DRY_VERSION=0.0.0-dry-run

log() { echo "release: $*" >&2; }
die() {
  echo "release: error: $*" >&2
  exit 1
}

kind="" bump="" version="" out="" display=staged image_as_is=0
signing="${VMLAB_RELEASE_SIGNING:-none}"
builder="${VMLAB_RELEASE_BUILDER:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --pre-release|--release) kind="$1"; shift ;;
    --image-as-is) image_as_is=1; shift ;;
    --bump|--version|--out|--signing|--builder|--display)
      [ $# -ge 2 ] || die "$1 needs a value"
      case "$1" in
        --bump) bump="$2" ;;
        --version) version="${2#v}" ;;
        --out) out="$2" ;;
        --signing) signing="$2" ;;
        --builder) builder="$2" ;;
        --display) display="$2" ;;
      esac
      shift 2 ;;
    -h|--help) sed -n '2,46p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done
case "$signing" in key|none) ;; *) die "--signing is key or none, not '$signing'" ;; esac
case "$display" in staged|rebuild|unsigned) ;; *) die "--display is staged, rebuild or unsigned, not '$display'" ;; esac

# ── 1. Version and commit ────────────────────────────────────────────────────
[ -z "$(git status --porcelain --untracked-files=no)" ] \
  || die "tracked files have uncommitted changes: commit them, the provenance names HEAD"
commit="$(git rev-parse HEAD)"
if [ -n "$version" ]; then
  [ -z "$kind" ] && [ -z "$bump" ] || die "--version does not go with a kind or --bump"
else
  [ -n "$kind" ] || die "give --pre-release or --release (with --bump), or --version X"
  version="$(./scripts/release-version.sh "$kind" ${bump:+--bump "$bump"} | sed -n 's/^version=//p')"
fi
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$ ]] || die "invalid version: '$version'"
if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
  die "the tag v$version exists already"
fi
[ -n "$builder" ] || builder="$(hostname -s 2>/dev/null || uname -n)"
out="${out:-$ROOT/target/release-local/v$version}"
log "version $version at $commit, built on $builder, into $out"

# ── Volumes of this release alone ────────────────────────────────────────────
# build/run.sh keeps the cargo home in one volume that every container build
# on the host shares, and the target dirs in volumes named after the path of
# the checkout. Code of an earlier build (a build.rs or a test of a card
# branch) can write to both: a crate source that cargo does not check again,
# or an output that cargo takes as fresh. A release takes neither. Its volumes
# have a random name, must not exist yet, and go away when this script ends.
release_volumes="vmlab-release-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
[[ "$release_volumes" =~ ^vmlab-release-[0-9a-f]{16}$ ]] || die "no random volume name: '$release_volumes'"
own_volumes() { docker volume ls -q | grep "^$release_volumes-" || true; }
[ -z "$(own_volumes)" ] || die "the docker volumes $release_volumes-* exist already: run again"
export VMLAB_BUILD_VOLUMES="$release_volumes"
remove_volumes() {
  local names
  names="$(own_volumes)"
  [ -z "$names" ] && return 0
  # shellcheck disable=SC2086  # one volume name per word
  docker volume rm $names >/dev/null \
    || log "warning: could not remove the docker volumes $release_volumes-*: remove them by hand"
}
trap remove_volumes EXIT

# ── 2. Build image and Windows display driver ────────────────────────────────
# The image first: the display driver compiles in it too. build/run.sh builds
# a missing image, and never rebuilds one that exists.
[ "$image_as_is" -eq 1 ] || just buildbox::image
container_env=()
case "$display" in
  unsigned)
    [ "$version" = "$DRY_VERSION" ] || die "--display unsigned is for the version $DRY_VERSION only"
    unsigned="$ROOT/guest/vdisplay-windows/target/unsigned-package"
    log "compiling the Windows display driver without a signature (dry run)"
    ./build/run.sh python3 guest/vdisplay-windows/build.py --output "$unsigned"
    container_env=(env VMLAB_RELEASE_DRY_RUN=1 "VMLAB_VDISPLAY_PACKAGE_DIR=$unsigned")
    ;;
  rebuild)
    log "building and test-signing the Windows display driver in the build lab"
    python3 guest/vdisplay-windows/rebuild.py --route container \
      ${VMLAB_RELEASE_VMLAB:+--vmlab "$VMLAB_RELEASE_VMLAB"}
    ;;
esac
if [ "$display" != unsigned ]; then
  package="${VMLAB_VDISPLAY_PACKAGE_DIR:-$ROOT/guest/vdisplay-windows/target/signed-package}"
  [ -s "$package/PACKAGE.json" ] || die "no signed Windows display package at $package.
Stage one with --display rebuild (needs the Windows 11 build lab, see
guest/vdisplay-windows/README.md), or point VMLAB_VDISPLAY_PACKAGE_DIR at one."
  case "$package" in
    "$ROOT"/*) ;;
    *) die "the display package must be inside the checkout, which is all the container sees: $package" ;;
  esac
  container_env=(env "VMLAB_VDISPLAY_PACKAGE_DIR=$package")
fi

# ── 3. Build in the container ────────────────────────────────────────────────
image="$(docker image inspect -f '{{.Id}}' "${VMLAB_BUILD_IMAGE:-vmlab-build}")"
./build/run.sh "${container_env[@]}" ./build/dist.sh "$version"

# ── 4. Collect, checksum, provenance ─────────────────────────────────────────
dist="$ROOT/target/dist"
assets=("vmlab-$version-linux-x86_64" "vmlab-fastpath-$version-linux-x86_64"
        "vmlab-guest-$version.tar.gz" "vmlab-hv-$version-linux-x86_64.tar.gz")
[ -n "$out" ] && [ "$out" != / ] || die "the release directory is empty or /"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
[ -n "$out" ] && [ "$out" != / ] || die "the release directory is empty or /"
# Only what a release holds is replaced: nothing else in --out is touched.
for name in "${assets[@]}" "vmlab-guest-$version.tar.gz.sha256" \
  "vmlab-hv-$version-linux-x86_64.tar.gz.sha256" SHA256SUMS SHA256SUMS.bundle \
  SHA256SUMS.key.bundle provenance.json; do
  rm -f "$out/$name"
done
for name in "${assets[@]}"; do
  [ -s "$dist/$name" ] || die "the build left no $name in $dist"
  cp -p "$dist/$name" "$out/$name"
done
for name in "vmlab-guest-$version.tar.gz" "vmlab-hv-$version-linux-x86_64.tar.gz"; do
  [ -s "$dist/$name.sha256" ] || die "the build left no $name.sha256 in $dist"
  cp -p "$dist/$name.sha256" "$out/$name.sha256"
  (cd "$out" && sha256sum --check --quiet "$name.sha256")
done
python3 scripts/release-assets.py prepare "$out" "$version" --commit "$commit" \
  --builder "$builder" --image "$image"

# ── 5. Sign ──────────────────────────────────────────────────────────────────
./scripts/release-sign.sh "$out" "$signing"

# ── 6. Check ─────────────────────────────────────────────────────────────────
python3 scripts/release-assets.py verify "$out" >/dev/null
(cd "$out" && sha256sum --check --quiet SHA256SUMS)
reported="$("$out/vmlab-$version-linux-x86_64" --version)"
case "$reported" in
  *"$version"*) ;;
  *) die "the CLI reports '$reported', not the version $version" ;;
esac

log "release directory $out:"
(cd "$out" && for name in *; do
  printf '%12s  %s  %s\n' "$(stat -c %s "$name")" "$(sha256sum "$name" | cut -d' ' -f1)" "$name"
done)
if [ "$version" = "$DRY_VERSION" ]; then
  log "dry run: this directory is for checking the build, and cannot be published"
else
  log "to publish, after the release gate (docs/agents/release-gate.md):"
  ./scripts/release-publish.sh "$out" --print
fi
