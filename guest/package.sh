#!/usr/bin/env bash
# Build every guest asset vmlab ships and pack them as the release bundle
# install.sh deploys: vmlab-guest-<version>.tar.gz plus a sha256sum-format
# vmlab-guest-<version>.tar.gz.sha256 beside it.
#
# The tarball's top level is the guest directory itself — exactly the layout
# src/guest_asset.rs and src/agent_asset.rs look up under
# ~/.local/share/vmlab/guest (or /usr/share/vmlab/guest):
#
#   x86_64/{vmlinuz,initramfs.img,VERSION}      container micro-VM asset
#   aarch64/{vmlinuz,initramfs.img,VERSION}
#   agent/<key>/<binary> + VERSION              every agent target
#   firmware/<arch>/*.fd + VERSION, firmware/VERSION
#                                               the UEFI firmware vmlab ships
#
# Strict by default (VMLAB_REQUIRE_ALL_TARGETS=1): a target whose toolchain is
# missing fails the package instead of being left out. guest/dist is wiped
# first, so nothing from an earlier partial build can ride along.
#
# Usage: guest/package.sh [version] [out-dir]
#   version  default: the package version in Cargo.toml
#   out-dir  default: target/guest-package

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$SCRIPT_DIR/dist"

die() {
  echo "package: error: $*" >&2
  exit 1
}

log() {
  echo "package: $*" >&2
}

# Every file the bundle must hold. A build that "succeeded" without one of
# these is a packaging failure, whatever the build scripts said.
EXPECTED=(
  x86_64/vmlinuz x86_64/initramfs.img x86_64/VERSION
  aarch64/vmlinuz aarch64/initramfs.img aarch64/VERSION
  agent/linux-x86_64/vmlab-agent agent/linux-x86_64/VERSION
  agent/linux-aarch64/vmlab-agent agent/linux-aarch64/VERSION
  agent/linux-riscv64/vmlab-agent agent/linux-riscv64/VERSION
  agent/linux-x86/vmlab-agent agent/linux-x86/VERSION
  agent/windows-x86_64/vmlab-agent.exe agent/windows-x86_64/VERSION
  agent/windows-x86/vmlab-agent.exe agent/windows-x86/VERSION
  agent/windows-nt-x86/vmlab-agent-legacy.exe agent/windows-nt-x86/VERSION
  agent/windows-9x-x86/vmlab-agent-legacy.exe agent/windows-9x-x86/VERSION
  agent/dos-i386/VMLABAGT.EXE agent/dos-i386/VERSION
  agent/linux-x86/vmlab-agent-legacy agent/linux-x86/VERSION-legacy
  agent/templeos/VmlabAgt.HC agent/templeos/VERSION
  firmware/VERSION
  firmware/x86_64/OVMF_CODE_4M.fd firmware/x86_64/OVMF_VARS_4M.fd
  firmware/x86_64/OVMF_CODE_4M.secboot.fd firmware/x86_64/OVMF_VARS_4M.ms.fd
  firmware/x86_64/VERSION
  firmware/aarch64/AAVMF_CODE.fd firmware/aarch64/AAVMF_VARS.fd
  firmware/aarch64/AAVMF_CODE.secboot.fd firmware/aarch64/AAVMF_VARS.ms.fd
  firmware/aarch64/VERSION
)

main() {
  local version="${1:-}" out="${2:-$ROOT/target/guest-package}"
  [[ -n "$version" ]] || version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n1)"
  [[ -n "$version" ]] || die "no version given and none found in Cargo.toml"
  version="${version#v}"
  export VMLAB_REQUIRE_ALL_TARGETS="${VMLAB_REQUIRE_ALL_TARGETS:-1}"

  rm -rf "$DIST_DIR"
  "$SCRIPT_DIR/build-asset.sh" x86_64 aarch64
  "$SCRIPT_DIR/build-agent.sh"
  # After build-agent.sh: the legacy linux-x86 build shares that directory.
  "$SCRIPT_DIR/build-agent-legacy.sh"

  local f missing=()
  for f in "${EXPECTED[@]}"; do
    [[ -s "$DIST_DIR/$f" ]] || missing+=("$f")
  done
  [[ ${#missing[@]} -eq 0 ]] || die "built set is incomplete, missing: ${missing[*]}"

  mkdir -p "$out"
  local name="vmlab-guest-$version.tar.gz"
  # Reproducible-ish: fixed order, owner and mode bits; no host user leaks in.
  # --sparse: the AAVMF images are 64 MiB of mostly zeros, and an extract
  # keeps the holes.
  tar -C "$DIST_DIR" --sort=name --owner=0 --group=0 --numeric-owner --sparse \
    --mtime="@${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || echo 0)}" \
    -cf - x86_64 aarch64 agent firmware | gzip -9n >"$out/$name"
  (cd "$out" && sha256sum "$name" >"$name.sha256")
  log "$(du -h "$out/$name" | cut -f1) → $out/$name"
  log "$(cat "$out/$name.sha256")"
}

main "$@"
