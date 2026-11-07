#!/usr/bin/env bash
# Build branded OVMF CODE; Debian remains the source of every VARS template.
# Run through build/run.sh in the vmlab-build container.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="$SCRIPT_DIR/.cache/edk2"
OUT="$SCRIPT_DIR/dist/firmware/x86_64"
DEBIAN_VERSION="2026.08+ds-2"
EDK2_TAG="edk2-stable202608"
EDK2_COMMIT="2970e5699ba6267f3384ffab20f96647578aebc8"
# Debian's repack of this commit includes the pinned submodules.
ORIG="edk2_2026.08+ds.orig.tar.xz"
ORIG_SHA256="ac1c2ba6dc2f7bdfc197668a6addfc2e61736ade9182b298e54012c9b6d7e424"
DEBIAN="edk2_$DEBIAN_VERSION.debian.tar.xz"
DEBIAN_SHA256="35ff83d0b1fe7d1804ae6cd3c2e5673168ddc6750c745977e048a494bcba502b"
export SOURCE_DATE_EPOCH=1786522436
export TZ=UTC LC_ALL=C PYTHON3_ENABLE=TRUE PYTHONHASHSEED=0
umask 022

die() { echo "build-ovmf: error: $*" >&2; exit 1; }
log() { echo "build-ovmf: $*" >&2; }

fetch() {
  local name="$1" sha="$2" snapshot="$3"
  if [[ ! -f "$CACHE/$name" ]]; then
    curl -fSL --retry 3 "https://deb.debian.org/debian/pool/main/e/edk2/${name//+/%2B}" \
      -o "$CACHE/$name.tmp" ||
      curl -fSL --retry 3 "https://snapshot.debian.org/file/$snapshot" -o "$CACHE/$name.tmp"
    [ -n "$CACHE" ] && [ "$CACHE" != / ] || { echo "VML-88: CACHE is empty or /" >&2; exit 1; }
    mv "$CACHE/$name.tmp" "$CACHE/$name"
  fi
  echo "$sha  $CACHE/$name" | sha256sum -c --quiet - || die "digest mismatch: $name"
}

main() {
  local tool
  for tool in curl tar xz patch make gcc g++ nasm iasl python3 sha256sum dpkg-buildflags; do
    command -v "$tool" >/dev/null || die "missing $tool; run through the build container: ./build/run.sh bash guest/build-firmware.sh x86_64"
  done
  [[ -s "$OUT/OVMF_VARS_4M.fd" && -s "$OUT/OVMF_VARS_4M.ms.fd" ]] ||
    die "run guest/fetch-firmware.sh x86_64 first"
  grep -qx "ovmf-generic=$DEBIAN_VERSION" "$OUT/VERSION" ||
    die "Debian VARS version differs from this build's source pin"
  mkdir -p "$CACHE"
  fetch "$ORIG" "$ORIG_SHA256" 26b37cdbdecda731c4dce01705f70b457d7c12fa
  fetch "$DEBIAN" "$DEBIAN_SHA256" 74f040449371006e44653e23eb331dd620652fa7

  local work
  work="$(mktemp -d "$CACHE/build.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$work'" EXIT
  tar -xJf "$CACHE/$ORIG" --strip-components=1 -C "$work"
  tar -xJf "$CACHE/$DEBIAN" -C "$work"
  local patch
  while IFS= read -r patch; do
    [[ -z "$patch" || "$patch" == \#* ]] && continue
    patch -d "$work" -p1 <"$work/debian/patches/$patch"
  done <"$work/debian/patches/series"
  # The upstream DSC uses CRLF; normalize it before our LF patch.
  sed -i 's/\r$//' "$work/OvmfPkg/Include/Dsc/NetworkComponents.dsc.inc"
  patch -d "$work" -p1 <"$SCRIPT_DIR/ovmf/reproducible-iscsi-pcd.patch"
  sed -i 's/\r$//' "$work/OvmfPkg/Library/PlatformBootManagerLib/BdsPlatform.c"
  patch -d "$work" -p1 <"$SCRIPT_DIR/ovmf/boot-without-waits.patch"
  sed -i 's/\r$//' "$work/OvmfPkg/PlatformDxe/Platform.c" "$work/OvmfPkg/PlatformDxe/Platform.inf"
  patch -d "$work" -p1 <"$SCRIPT_DIR/ovmf/host-resolution.patch"
  sed -i 's/\r$//' "$work/BaseTools/Source/C/GenFw/Elf64Convert.c"
  patch -d "$work" -p1 <"$SCRIPT_DIR/ovmf/reproducible-debug-path.patch"
  sed -i 's/\r$//' "$work/OvmfPkg/OvmfPkgX64.dsc"
  patch -d "$work" -p1 <"$SCRIPT_DIR/ovmf/reproducible-source-path.patch"
  [ -n "$SCRIPT_DIR" ] && [ "$SCRIPT_DIR" != / ] || { echo "VML-88: SCRIPT_DIR is empty or /" >&2; exit 1; }
  cp "$SCRIPT_DIR/logo/Logo.bmp" "$work/MdeModulePkg/Logo/Logo.bmp"

  (
    cd "$work"
    unset WORKSPACE EDK_TOOLS_PATH CONF_PATH ECP_SOURCE EDK_SOURCE EFI_SOURCE
    export DEB_BUILD_MAINT_OPTIONS=hardening=+all
    EXTRA_OPTFLAGS="$(dpkg-buildflags --get CFLAGS)"
    EXTRA_LDFLAGS="$(dpkg-buildflags --get LDFLAGS)"
    export EXTRA_OPTFLAGS EXTRA_LDFLAGS
    # edksetup uses unset variables internally.
    set +u
    # shellcheck disable=SC1091
    . ./edksetup.sh
    set -u
    make -C BaseTools -j "${VMLAB_OVMF_JOBS:-8}"
    python3 "$SCRIPT_DIR/ovmf/test-genfw-reproducible.py" "$work/BaseTools/Source/C/bin/GenFw"
    local mode
    local -a flags=(
      -a X64 -t GCC -p OvmfPkg/OvmfPkgX64.dsc
      -n "${VMLAB_OVMF_JOBS:-8}"
      -DCC_MEASUREMENT_ENABLE=TRUE -DNETWORK_HTTP_BOOT_ENABLE=TRUE
      -DNETWORK_IP6_ENABLE=TRUE -DNETWORK_TLS_ENABLE -DTPM2_ENABLE=TRUE
      -DFD_SIZE_4MB --pcd PcdUninstallMemAttrProtocol=TRUE
      --pcd PcdPlatformBootTimeOut=0
      --pcd 'PcdFirmwareVendor=Lvmlab\0'
      --pcd 'PcdFirmwareVersionString=L2026.08+ds-2-vmlab\0'
      --pcd 'PcdFirmwareReleaseDateString=L08/12/2026\0'
    )
    local network target
    for target in RELEASE DEBUG; do
    for network in ipxe native; do
    for mode in plain secboot; do
      local -a extra=()
      [[ "$mode" == plain ]] || extra=(-DBUILD_SHELL=FALSE -DSECURE_BOOT_ENABLE=TRUE -DSMM_REQUIRE=TRUE)
      if [[ "$network" == ipxe ]]; then
        extra+=(-DNETWORK_ENABLE=FALSE)
      else
        extra+=(-DNETWORK_ALLOW_HTTP_CONNECTIONS=TRUE)
      fi
      log "building $target/$mode/$network from $EDK2_TAG ($EDK2_COMMIT)"
      {
        printf '%s/%s/%s:' "$target" "$mode" "$network"
        printf ' %q' "${flags[@]}" -b "$target" "${extra[@]}"
        printf '\n'
      } >>"$work/flags.txt"
      build "${flags[@]}" -b "$target" "${extra[@]}"
      local fv="Build/OvmfX64/${target}_GCC/FV" dest=OVMF_CODE_4M.fd
      [[ "$mode" == plain ]] || dest=OVMF_CODE_4M.secboot.fd
      [[ "$network" == ipxe ]] || dest="${dest%.fd}.network.fd"
      [[ "$target" == RELEASE ]] || dest="${dest%.fd}.debug.fd"
      [[ "$(stat -c %s "$fv/OVMF_CODE.fd")" == 3653632 ]] || die "unexpected CODE flash size"
      [[ "$(stat -c %s "$fv/OVMF_VARS.fd")" == 540672 ]] || die "unexpected VARS flash size"
      cmp "$fv/OVMF_VARS.fd" "$OUT/OVMF_VARS_4M.fd" || die "VARS layout/content differs from Debian"
      [ -n "$fv" ] && [ "$fv" != / ] || { echo "VML-88: fv is empty or /" >&2; exit 1; }
      install -m 0644 "$fv/OVMF_CODE.fd" "$work/$dest"
      # Defines change between modes; never reuse modules from the other build.
      rm -rf Build
    done
    done
    done
  )
  [ -n "$OUT" ] && [ "$OUT" != / ] || { echo "VML-88: OUT is empty or /" >&2; exit 1; }
  install -m 0644 "$work/OVMF_CODE_4M"*.fd "$OUT/"
  printf '{"boot_hints":true,"gop_resolution":true}\n' >"$OUT/features.json"
  {
    printf 'edk2_tag=%s\nedk2_commit=%s\ndebian_source=%s\nsource_date_epoch=%s\n' \
      "$EDK2_TAG" "$EDK2_COMMIT" "$DEBIAN_VERSION" "$SOURCE_DATE_EPOCH"
    printf 'source_sha256=%s\ndebian_sha256=%s\n' "$ORIG_SHA256" "$DEBIAN_SHA256"
    cat "$work/flags.txt"
    gcc --version | head -n1
    ld --version | head -n1
    nasm -v
    iasl -v
    python3 --version
    sha256sum "$SCRIPT_DIR/build-ovmf.sh" | sed "s|$SCRIPT_DIR/||"
    sha256sum "$SCRIPT_DIR/ovmf/reproducible-iscsi-pcd.patch" | sed "s|$SCRIPT_DIR/||"
    sha256sum "$SCRIPT_DIR/ovmf/boot-without-waits.patch" | sed "s|$SCRIPT_DIR/||"
    sha256sum "$SCRIPT_DIR/ovmf/host-resolution.patch" | sed "s|$SCRIPT_DIR/||"
    sha256sum "$SCRIPT_DIR/ovmf/reproducible-debug-path.patch" | sed "s|$SCRIPT_DIR/||"
    sha256sum "$SCRIPT_DIR/ovmf/reproducible-source-path.patch" | sed "s|$SCRIPT_DIR/||"
    sha256sum "$SCRIPT_DIR/ovmf/test-genfw-reproducible.py" | sed "s|$SCRIPT_DIR/||"
    sha256sum "$SCRIPT_DIR/logo/Logo.bmp" | sed "s|$SCRIPT_DIR/||"
    (cd "$OUT" && sha256sum OVMF_CODE_4M*.fd OVMF_VARS_4M*.fd)
  } >"$OUT/BUILDINFO"
  log "built CODE and BUILDINFO in $OUT; Debian VARS unchanged"
}

main "$@"
