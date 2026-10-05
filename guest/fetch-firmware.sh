#!/usr/bin/env bash
# Fetch the UEFI firmware vmlab ships (PRD §5.2) into
# guest/dist/firmware/<arch>/, beside the micro-VM asset, so a host needs no
# OVMF package and every host boots the same bytes. src/qemu/firmware.rs looks
# it up under <guest asset dir>/firmware/<arch>/ before the host's own.
#
#   x86_64   Debian `ovmf`:
#              OVMF_CODE_4M.fd          + OVMF_VARS_4M.fd      plain UEFI
#              OVMF_CODE_4M.secboot.fd  + OVMF_VARS_4M.ms.fd   secure boot,
#                                         Microsoft + distro keys enrolled
#   aarch64  Debian `qemu-efi-aarch64`:
#              AAVMF_CODE.fd            + AAVMF_VARS.fd        plain UEFI
#              AAVMF_CODE.secboot.fd    + AAVMF_VARS.ms.fd     secure boot
#
# Each .deb is pinned by exact version + sha256 and fetched from
# deb.debian.org, falling back to snapshot.debian.org once the pool has
# dropped the version (a stable point release supersedes it). A .deb is an ar
# archive; its data.tar.xz is unpacked without root. The AAVMF images are
# 64 MiB of mostly zero padding, so they are copied sparse.
#
# Usage: guest/fetch-firmware.sh [arch...]   (default: x86_64 aarch64)

set -euo pipefail

MIRROR="${VMLAB_DEBIAN_MIRROR:-https://deb.debian.org/debian}"
SNAPSHOT="https://snapshot.debian.org"

# Pinned packages: arch|package|version|sha256 — all from the edk2 source
# package, Debian 13 (trixie).
PACKAGES=(
  "x86_64|ovmf|2025.02-8+deb13u1|78e0d54df11fc77406cb7a0bc9a39e5bca6d1cbe06556b91d9a73491c52decdf"
  "aarch64|qemu-efi-aarch64|2025.02-8+deb13u1|a00b2411a79c8aeafd95a7c868ac3cd1aab592f1af9fff965f02d81a625276ed"
)

# What each arch ships: destination name|path inside the package. The
# destination names are the layout src/qemu/firmware.rs reads.
files_for() {
  case "$1" in
    x86_64)
      echo "OVMF_CODE_4M.fd|usr/share/OVMF/OVMF_CODE_4M.fd"
      echo "OVMF_VARS_4M.fd|usr/share/OVMF/OVMF_VARS_4M.fd"
      echo "OVMF_CODE_4M.secboot.fd|usr/share/OVMF/OVMF_CODE_4M.secboot.fd"
      echo "OVMF_VARS_4M.ms.fd|usr/share/OVMF/OVMF_VARS_4M.ms.fd"
      ;;
    aarch64)
      echo "AAVMF_CODE.fd|usr/share/AAVMF/AAVMF_CODE.no-secboot.fd"
      echo "AAVMF_VARS.fd|usr/share/AAVMF/AAVMF_VARS.fd"
      echo "AAVMF_CODE.secboot.fd|usr/share/AAVMF/AAVMF_CODE.secboot.fd"
      echo "AAVMF_VARS.ms.fd|usr/share/AAVMF/AAVMF_VARS.ms.fd"
      ;;
    *) die "no bundled firmware for arch '$1' (supported: x86_64 aarch64)" ;;
  esac
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="$SCRIPT_DIR/.cache/deb"
OUT_DIR="$SCRIPT_DIR/dist/firmware"

die() {
  echo "fetch-firmware: error: $*" >&2
  exit 1
}

log() {
  echo "fetch-firmware: $*" >&2
}

need() {
  local tool
  for tool in "$@"; do
    command -v "$tool" >/dev/null 2>&1 || die "missing host tool: $tool"
  done
}

# Download (once, into the cache) and checksum-verify one .deb. Prints the
# cached file path.
fetch_deb() {
  local pkg="$1" ver="$2" sha="$3"
  local file="$CACHE_DIR/${pkg}_${ver}_all.deb"
  if [[ ! -f "$file" ]]; then
    log "fetching $pkg $ver"
    local url="$MIRROR/pool/main/e/edk2/${pkg}_${ver//+/%2B}_all.deb"
    if ! curl -fsSL -o "$file.tmp" "$url"; then
      # The pool drops a stable version at the next point release; the
      # snapshot archive keeps every one, addressed by its sha1.
      log "$pkg $ver is gone from $MIRROR — trying snapshot.debian.org"
      local hash
      hash="$(curl -fsSL "$SNAPSHOT/mr/binary/$pkg/${ver//+/%2B}/binfiles" 2>/dev/null \
        | grep -oE '"hash":"[0-9a-f]{40}"' | head -n1 | cut -d'"' -f4)" || true
      [[ -n "$hash" ]] || die "$pkg $ver: not on $MIRROR and not found on snapshot.debian.org"
      curl -fsSL -o "$file.tmp" "$SNAPSHOT/file/$hash" \
        || die "download failed: $SNAPSHOT/file/$hash ($pkg $ver)"
    fi
    mv "$file.tmp" "$file"
  fi
  echo "$sha  $file" | sha256sum -c --quiet - >/dev/null 2>&1 \
    || die "sha256 mismatch for $file (delete it to re-download)"
  echo "$file"
}

fetch_arch() {
  local arch="$1" entry a pkg ver sha deb
  for entry in "${PACKAGES[@]}"; do
    IFS='|' read -r a pkg ver sha <<<"$entry"
    [[ "$a" == "$arch" ]] || continue
    deb="$(fetch_deb "$pkg" "$ver" "$sha")"

    local work
    work="$(mktemp -d "${TMPDIR:-/tmp}/vmlab-firmware.XXXXXX")"
    # shellcheck disable=SC2064  # expand $work now, not at trap time
    trap "rm -rf '$work'" RETURN
    (cd "$work" && ar x "$deb" data.tar.xz) || die "not a .deb with data.tar.xz: $deb"
    tar -xJf "$work/data.tar.xz" -C "$work" || die "unpack failed: $deb"

    local out="$OUT_DIR/$arch" line dest src
    rm -rf "$out"
    mkdir -p "$out"
    while IFS= read -r line; do
      dest="${line%%|*}"
      src="${line#*|}"
      [[ -f "$work/$src" ]] || die "$pkg $ver has no $src"
      cp --sparse=always --dereference "$work/$src" "$out/$dest"
      chmod 0644 "$out/$dest"
    done < <(files_for "$arch")
    printf '%s=%s\n' "$pkg" "$ver" >"$out/VERSION"
    log "$arch: $pkg $ver → $out"
  done
}

# The manifest at firmware/VERSION: every pin, whichever arches were built.
write_manifest() {
  local entry a pkg ver sha
  : >"$OUT_DIR/VERSION"
  for entry in "${PACKAGES[@]}"; do
    IFS='|' read -r a pkg ver sha <<<"$entry"
    printf '%s %s=%s sha256=%s\n' "$a" "$pkg" "$ver" "$sha" >>"$OUT_DIR/VERSION"
  done
}

main() {
  need curl ar tar xz sha256sum cp
  mkdir -p "$CACHE_DIR" "$OUT_DIR"
  local -a arches=("$@")
  [[ ${#arches[@]} -gt 0 ]] || arches=(x86_64 aarch64)
  local arch
  for arch in "${arches[@]}"; do
    files_for "$arch" >/dev/null
    fetch_arch "$arch"
  done
  write_manifest
}

main "$@"
