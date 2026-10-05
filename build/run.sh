#!/usr/bin/env bash
# Run a command in the vmlab-build container over this checkout, as the host
# user, so nothing it writes into the checkout ends up owned by root.
#
#   - The checkout is bind-mounted at its own absolute path, and so is the git
#     common dir when it lives elsewhere (a worktree's .git file points there),
#     so git works inside: package.sh and the version stamps read it.
#   - Named volumes hold the cargo home (shared by every checkout: registry and
#     git cache) and, per checkout, every target dir the build writes — the host
#     crate's target/debug and target/release, the guest crates' and ebpf's
#     target/ — so rebuilds are incremental. They shadow the host's own build
#     dirs rather than share them: a host build and a container build never
#     invalidate each other. target/dist, target/guest-package and guest/dist
#     are not shadowed, so the artefacts land in the checkout.
#   - The container starts as root only to hand those volume roots to the host
#     uid:gid (a fresh volume is root-owned), then drops to that uid:gid with
#     setpriv before running the command.
#
# Usage: build/run.sh [-t] command [args...]
#   -t  allocate a terminal (interactive shell)
# Env: VMLAB_BUILD_IMAGE (default vmlab-build)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${VMLAB_BUILD_IMAGE:-vmlab-build}"

tty=()
if [[ "${1:-}" == "-t" ]]; then
  tty=(-it)
  shift
fi
[[ $# -gt 0 ]] || {
  echo "usage: build/run.sh [-t] command [args...]" >&2
  exit 2
}

# One set of target volumes per checkout: two worktrees must never share a
# target dir, since cargo judges freshness by mtime.
key="$(printf '%s' "$ROOT" | sha256sum | cut -c1-12)"

# slot:path relative to the checkout
TARGETS=(
  host-debug:target/debug
  host-release:target/release
  cinit:guest/cinit/target
  cinit-proto:guest/cinit-proto/target
  agent:guest/agent/target
  agent-proto:guest/agent-proto/target
  ebpf:ebpf/target
)

mounts=(-v "$ROOT:$ROOT" -v "vmlab-build-cargo:/var/cache/vmlab-build/cargo")
owned=(/var/cache/vmlab-build/cargo)
for t in "${TARGETS[@]}"; do
  slot="${t%%:*}" rel="${t#*:}"
  # Created here, as the host user: docker would create a missing mount point
  # inside the bind mount as root.
  mkdir -p "$ROOT/$rel"
  mounts+=(-v "vmlab-build-$key-$slot:$ROOT/$rel")
  owned+=("$ROOT/$rel")
done

common="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
if [[ -n "$common" && "$common" != "$ROOT"/* ]]; then
  mounts+=(-v "$common:$common")
fi

env_args=(-e TERM="${TERM:-xterm}" -e VMLAB_BUILD_IDS="$(id -u):$(id -g)" -e VMLAB_BUILD_OWNED="${#owned[@]}")
[[ -z "${SOURCE_DATE_EPOCH:-}" ]] || env_args+=(-e SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH")

# Root for the chown alone; everything after setpriv runs as the host uid:gid.
# shellcheck disable=SC2016  # expanded inside the container
exec docker run --rm --init "${tty[@]}" "${mounts[@]}" -w "$ROOT" "${env_args[@]}" \
  "$IMAGE" bash -c '
    set -e
    ids="$VMLAB_BUILD_IDS" n="$VMLAB_BUILD_OWNED"
    unset VMLAB_BUILD_IDS VMLAB_BUILD_OWNED
    for d in "${@:1:$n}"; do
      [ "$(stat -c %u:%g "$d")" = "$ids" ] || chown "$ids" "$d"
    done
    shift "$n"
    exec setpriv --reuid="${ids%:*}" --regid="${ids#*:}" --clear-groups -- "$@"
  ' bash "${owned[@]}" "$@"
