#!/usr/bin/env bash
# Build firmware in the release toolchain environment, using the host uid.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Set by build/run.sh, the image itself, and CI after provisioning toolchains.
# A generic container marker would also match containers without these tools.
if [[ "${VMLAB_BUILD_ENV:-}" != 1 ]]; then
  exec "$ROOT/build/run.sh" bash "$SCRIPT_DIR/build-firmware.sh" "$@"
fi

arches=("$@")
[[ ${#arches[@]} -gt 0 ]] || arches=(x86_64 aarch64)
"$SCRIPT_DIR/fetch-firmware.sh" "${arches[@]}"
for arch in "${arches[@]}"; do
  if [[ "$arch" == x86_64 ]]; then
    "$SCRIPT_DIR/build-ovmf.sh"
    python3 "$SCRIPT_DIR/ipxe/build.py"
    bash "$SCRIPT_DIR/build-seabios.sh"
  fi
done
