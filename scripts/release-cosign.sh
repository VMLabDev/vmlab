#!/usr/bin/env bash
# The one place a release step runs cosign (card 1021). cosign is in the
# vmlab-build image (build/Dockerfile, pinned by scripts/install-cosign.sh),
# so the build machine needs docker and no cosign of its own.
#
# Each path that cosign reads or writes is bind-mounted at its own absolute
# path, so the cosign arguments are the same inside and outside the container.
# The container has no network (unless --network), runs as the uid:gid of the
# caller, and is removed when cosign ends.
#
# The release key: --secrets hands VMLAB_RELEASE_COSIGN_KEY and COSIGN_PASSWORD
# to the container with `docker run -e NAME`, a name with no value. The value
# goes from the environment of this script to the environment of the container
# and is in no argument list, no file and no layer of the image. This script
# prints neither, and turns shell tracing off before it touches them.
#
# Usage: release-cosign.sh [options] -- <cosign arguments>
#        release-cosign.sh --check     say whether cosign can run here, run nothing
#   --rw DIR      a directory cosign writes into (absolute, existing)
#   --ro PATH     a file or directory cosign reads (absolute, existing)
#   --workdir DIR the working directory of cosign (one of the --rw directories)
#   --secrets     hand the two variables of the release key to cosign
#   --tty         attach the terminal (the password prompt of generate-key-pair)
#   --network     give the container the network (keyless verification reads
#                 the sigstore trust root)
# Env: COSIGN             run this cosign binary, and no container (the tests,
#                         and a host that has the pinned cosign)
#      VMLAB_BUILD_IMAGE  the image (default vmlab-build)

set -euo pipefail
set +x

IMAGE="${VMLAB_BUILD_IMAGE:-vmlab-build}"
SECRETS=(VMLAB_RELEASE_COSIGN_KEY COSIGN_PASSWORD)

die() {
  echo "release-cosign: error: $*" >&2
  exit 1
}

mounts=() workdir="" secrets=0 tty=0 network=none check=0
while [ $# -gt 0 ]; do
  case "$1" in
    --rw|--ro|--workdir)
      [ $# -ge 2 ] || die "$1 needs a path"
      case "$2" in
        /) die "$1 does not take /" ;;
        *:*) die "a path with ':' cannot be mounted: $2" ;;
        /*) ;;
        *) die "$1 takes an absolute path, not '$2'" ;;
      esac
      [ -e "$2" ] || die "no such path: $2"
      case "$1" in
        --rw) [ -d "$2" ] || die "--rw takes a directory: $2"; mounts+=(-v "$2:$2") ;;
        --ro) mounts+=(-v "$2:$2:ro") ;;
        --workdir) workdir="$2" ;;
      esac
      shift 2 ;;
    --check) check=1; shift ;;
    --secrets) secrets=1; shift ;;
    --tty) tty=1; shift ;;
    --network) network=bridge; shift ;;
    --) shift; break ;;
    *) die "unknown option: $1" ;;
  esac
done
[ $# -gt 0 ] || [ "$check" -eq 1 ] || die "usage: release-cosign.sh [options] -- <cosign arguments>"

if [ "$secrets" -eq 1 ]; then
  # The name only: a value never reaches a message.
  [ -n "${VMLAB_RELEASE_COSIGN_KEY:+set}" ] \
    || die "VMLAB_RELEASE_COSIGN_KEY is not set: it holds the private half of the release key (the content of cosign.key)"
  [ -n "${COSIGN_PASSWORD:+set}" ] \
    || die "COSIGN_PASSWORD is not set: it holds the password of the release key"
  export VMLAB_RELEASE_COSIGN_KEY COSIGN_PASSWORD
fi

# A cosign binary that the caller names, or the one of the build container
# when this script runs inside it.
direct=""
if [ -n "${COSIGN:-}" ]; then
  command -v "$COSIGN" >/dev/null 2>&1 || die "need cosign: COSIGN names '$COSIGN', which does not run"
  direct="$COSIGN"
elif [ "${VMLAB_BUILD_ENV:-}" = 1 ] && command -v cosign >/dev/null 2>&1; then
  direct=cosign
fi
if [ -n "$direct" ]; then
  [ "$check" -eq 0 ] || exit 0
  [ -z "$workdir" ] || cd "$workdir"
  exec "$direct" "$@"
fi

command -v docker >/dev/null 2>&1 \
  || die "need cosign: it runs in the $IMAGE container, and there is no docker on PATH (just release::doctor)"
docker image inspect "$IMAGE" >/dev/null 2>&1 \
  || die "need cosign: there is no image $IMAGE. Build it with \`just buildbox::image\`"
# The image says which cosign it holds (a label of build/Dockerfile), so an
# image from before card 1021 is found without starting a container.
[ -n "$(docker image inspect -f '{{index .Config.Labels "io.vmlab.cosign"}}' "$IMAGE" 2>/dev/null)" ] \
  || die "need cosign: the image $IMAGE has none. Build it again with \`just buildbox::image\`"
[ "$check" -eq 0 ] || exit 0

run=(run --rm --network "$network" --user "$(id -u):$(id -g)" --cap-drop ALL
     --security-opt no-new-privileges "${mounts[@]}")
[ -z "$workdir" ] || run+=(-w "$workdir")
if [ "$tty" -eq 1 ]; then
  [ -t 0 ] && [ -t 1 ] || die "this step asks for a password: run it in a terminal"
  run+=(-it)
fi
if [ "$secrets" -eq 1 ]; then
  for name in "${SECRETS[@]}"; do
    run+=(-e "$name")
  done
fi

exec docker "${run[@]}" "$IMAGE" cosign "$@"
