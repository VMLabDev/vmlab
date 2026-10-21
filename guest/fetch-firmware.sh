#!/usr/bin/env bash
# Fetch the UEFI firmware vmlab ships (PRD §5.2) into
# guest/dist/firmware/<arch>/, beside the micro-VM asset, so a host needs no
# OVMF package and every host boots the same bytes. src/qemu/firmware.rs looks
# it up under <guest asset dir>/firmware/<arch>/ before the host's own.
#
#   x86_64   Debian `ovmf-generic`:
#              OVMF_CODE_4M.fd          + OVMF_VARS_4M.fd      plain UEFI
#              OVMF_CODE_4M.secboot.fd  + OVMF_VARS_4M.ms.fd   secure boot,
#                                         Microsoft + distro keys enrolled
#   aarch64  Debian `qemu-efi-aarch64`:
#              AAVMF_CODE.fd            + AAVMF_VARS.fd        plain UEFI
#              AAVMF_CODE.secboot.fd    + AAVMF_VARS.ms.fd     secure boot
#
# `secure_boot_keys = "setup"` pairs each secboot CODE with the blank VARS
# above (setup mode), and `secure_boot_vars` boots a user file under it, so
# no extra image is needed for either.
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
# package, Debian unstable. Not trixie's 2025.02: its secure-boot VARS enrol
# only Microsoft's 2011 certificates, and a Windows boot manager signed by the
# "Windows UEFI CA 2023" (shipped by Windows updates from 2026-09) is refused
# with Access Denied by any fresh store made from them. These enrol both the
# 2011 and the 2023 Microsoft CAs. The x86_64 images moved from `ovmf` to
# `ovmf-generic`, which `ovmf` now only depends on.
PACKAGES=(
  "x86_64|ovmf-generic|2026.08+ds-2|05c1593998ba58700ff86c062bde306321af1f5a2943ea9ceef6c2332f3f3374"
  "aarch64|qemu-efi-aarch64|2026.08+ds-2|30467a3593c099ea42bd6ff3c4590e4b205edf9a7b98844f4bed17cd8b05eb58"
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
