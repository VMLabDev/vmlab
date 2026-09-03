#!/usr/bin/env bash
# Build a msvcrt-flavoured mingw-w64 runtime for the guest agents.
#
# Every Windows binary the distro toolchains produce imports the UCRT
# (api-ms-win-crt-*.dll), which ships with Windows 10 and later. On anything
# older the loader refuses the process outright — "api-ms-win-crt-convert-
# l1-1-0.dll is missing", seen live on Windows 7 — and the agent is not
# partially broken there, it does not start. Windows 7 RTM cannot even take the
# UCRT update: KB2999226 needs SP1, and its payload ships as PA30 servicing
# deltas rather than DLLs anyone can stage.
#
# msvcrt.dll, on the other hand, has been in Windows since NT4. mingw-w64 can
# target it — the distro packages are simply built the other way — so this
# builds only the CRT against msvcrt into a private prefix. It does not touch
# the system toolchain: the same gcc links against these objects when
# build-agent.sh points `-B`/`-L` at them.
#
# One-time, ~5 minutes. Needs the mingw-w64 gcc cross toolchain already
# installed (i686-w64-mingw32-gcc / x86_64-w64-mingw32-gcc).
#
# Usage: build-mingw-msvcrt.sh [version]     (default: matches mingw-w64 14.0.0)
set -euo pipefail

VERSION="${1:-14.0.0}"
PREFIX="${VMLAB_MSVCRT_PREFIX:-$HOME/.local/share/vmlab/toolchains/mingw-msvcrt}"
URL="https://downloads.sourceforge.net/project/mingw-w64/mingw-w64/mingw-w64-release/mingw-w64-v${VERSION}.tar.bz2"

log() { echo "build-mingw-msvcrt: $*" >&2; }
die() { echo "build-mingw-msvcrt: error: $*" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

log "fetching mingw-w64 $VERSION"
curl -fSL --retry 3 -o "$work/src.tar.bz2" "$URL" || die "download failed: $URL"
mkdir -p "$work/src"
tar xf "$work/src.tar.bz2" -C "$work/src" --strip-components=1

built=0
for host in i686-w64-mingw32 x86_64-w64-mingw32; do
    if ! command -v "$host-gcc" >/dev/null 2>&1; then
        log "skipping $host: $host-gcc not found (install mingw-w64)"
        continue
    fi
    log "building the msvcrt CRT for $host"
    mkdir -p "$work/build/$host"
    (
        cd "$work/build/$host"
        "$work/src/mingw-w64-crt/configure" \
            --host="$host" \
            --prefix="$PREFIX/$host" \
            --with-default-msvcrt=msvcrt \
            --with-sysroot="/usr/$host" \
            --disable-dependency-tracking >configure.log 2>&1 || {
                tail -20 configure.log >&2; exit 1; }
        make -j"$(nproc)" >build.log 2>&1 || { tail -20 build.log >&2; exit 1; }
        make install >install.log 2>&1 || { tail -20 install.log >&2; exit 1; }
    ) || die "$host build failed"
    log "$host: installed into $PREFIX/$host"
    built=$((built + 1))
done

[[ "$built" -gt 0 ]] || die "no mingw-w64 cross compiler found; nothing built"
log "done — build-agent.sh will pick this up from $PREFIX"
