#!/usr/bin/env bash
# Build the default Microsoft VARS without changing the pristine Debian copy.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$SCRIPT_DIR/dist/firmware/x86_64"
command -v uv >/dev/null || { echo "build-secureboot: missing uv; run scripts/guest-toolchains.sh uv" >&2; exit 1; }
work="$(mktemp -d "$OUT/secureboot.XXXXXX")"
# shellcheck disable=SC2064
trap "rm -rf '$work'" EXIT
uv run --frozen --script "$SCRIPT_DIR/enroll-secureboot.py" \
  --template "$OUT/OVMF_VARS_4M.ms.debian.fd" \
  --microsoft-manifest "$SCRIPT_DIR/secureboot/microsoft-2023.json" \
  --output "$work/vars.fd" >"$work/BUILDINFO.json"
[ -n "$work" ] && [ "$work" != / ] || { echo "VML-88: work is empty or /" >&2; exit 1; }
[ -n "$OUT" ] && [ "$OUT" != / ] || { echo "VML-88: OUT is empty or /" >&2; exit 1; }
install -m 0644 "$work/vars.fd" "$OUT/OVMF_VARS_4M.ms.fd"
install -m 0644 "$work/BUILDINFO.json" "$OUT/SECUREBOOT-BUILDINFO.json"
