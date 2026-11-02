#!/usr/bin/env bash
# Build SeaBIOS independently of QEMU's source and firmware packages.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="$SCRIPT_DIR/.cache/seabios"
OUT="$SCRIPT_DIR/dist/firmware/x86_64"
VERSION=1.17.0
COMMIT=b52ca86e094d19b58e2304417787e96b940e39c6
SHA256=5159a35fac5c1579dd5d702001033c826ce45d30d3bd657ec1b2695455997712
export SOURCE_DATE_EPOCH=1749578690 LC_ALL=C TZ=UTC PYTHONDONTWRITEBYTECODE=1
umask 022
mkdir -p "$CACHE" "$OUT"
archive="$CACHE/seabios-$VERSION.tar.gz"
if [[ ! -f "$archive" ]]; then
  curl -fSL --retry 3 "https://www.seabios.org/downloads/seabios-$VERSION.tar.gz" -o "$archive.tmp"
  mv "$archive.tmp" "$archive"
fi
echo "$SHA256  $archive" | sha256sum -c --quiet -
work="$(mktemp -d "$CACHE/build.XXXXXX")"
# shellcheck disable=SC2064
trap "rm -rf '$work'" EXIT
tar -xzf "$archive" --strip-components=1 -C "$work"
patch -d "$work" -p1 <"$SCRIPT_DIR/seabios/splash-during-post.patch"
for variant in q35 legacy; do
  cp "$SCRIPT_DIR/seabios/$variant.config" "$work/.config"
  make -C "$work" PYTHON=python3 EXTRAVERSION=-vmlab olddefconfig
  make -C "$work" -j "${VMLAB_FIRMWARE_JOBS:-8}" PYTHON=python3 EXTRAVERSION=-vmlab
  test "$(stat -c %s "$work/out/bios.bin")" = 262144
  install -m 0644 "$work/out/bios.bin" "$OUT/bios-vmlab-$variant.bin"
  install -m 0644 "$work/.config" "$OUT/seabios-$variant.config"
  make -C "$work" clean
done
install -m 0644 "$work/COPYING" "$OUT/SeaBIOS-COPYING"
install -m 0644 "$work/COPYING.LESSER" "$OUT/SeaBIOS-COPYING.LESSER"
# Include the exact corresponding source and local build inputs.
mkdir -p "$work/vmlab-build/seabios"
cp "$SCRIPT_DIR/build-seabios.sh" "$work/vmlab-build/"
cp "$SCRIPT_DIR/seabios/"* "$work/vmlab-build/seabios/"
tar -C "$work" --sort=name --owner=0 --group=0 --numeric-owner \
  --mtime="@$SOURCE_DATE_EPOCH" --exclude=out --exclude=.config --exclude=.config.old --exclude=__pycache__ \
  -cf - . | gzip -9n >"$OUT/seabios-source.tar.gz"
{
  printf 'version=%s\ncommit=%s\nsource_sha256=%s\nsource_date_epoch=%s\n' "$VERSION" "$COMMIT" "$SHA256" "$SOURCE_DATE_EPOCH"
  gcc --version | head -n1
  ld --version | head -n1
  sha256sum "$SCRIPT_DIR/build-seabios.sh" "$SCRIPT_DIR/seabios/"* | sed "s|$SCRIPT_DIR/||"
  (cd "$OUT" && sha256sum bios-vmlab-*.bin seabios-*.config seabios-source.tar.gz)
} >"$OUT/SEABIOS-BUILDINFO"
