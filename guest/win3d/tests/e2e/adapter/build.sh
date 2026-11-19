#!/usr/bin/env bash
# Build what the adapter suite (tests/e2e/adapter.sh) runs in the guest, in
# the shared toolchain image, x64 only:
#
#   tests/e2e/adapter/build.sh [wine-source-tarball]
#
# - the suite's own probes, probes/*_probe.cpp, into out/adapter/x64/
# - with a Wine source tarball (the wine pin in tests/e2e/PINS), Wine's
#   Direct3D 9, 10 and 11 tests into out/adapter/winetests/x64/, one program
#   per test file (-DSTANDALONE, wine/test.h). They are built against the
#   toolchain's mingw-w64 headers, with Wine's include directory searched
#   last, for wine/test.h and the few headers mingw-w64 does not have.
#
# The probes are not part of the driver package and are not checked by
# build/probes/check-imports.sh: they import d2d1.dll and dwmapi.dll.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
out=$repo/out/adapter
tarball=${1:-}
image=$("$repo/build/toolchain/build.sh")
mkdir -p "$out/x64"

docker run --rm --user "$(id -u):$(id -g)" -v "$repo:/src" -w /src/tests/e2e/adapter/probes "$image" bash -c '
  set -e
  for src in *_probe.cpp; do
    exe=/src/out/adapter/x64/${src%.cpp}.exe
    if [ "$exe" -nt "$src" ] && [ "$exe" -nt adapter.h ]; then continue; fi
    x86_64-w64-mingw32-clang++ -O2 -Wall -Wextra -Wno-unused-parameter -Wno-missing-field-initializers \
      -std=c++17 -fno-exceptions -fno-rtti -D_WIN32_WINNT=0x0A00 -DWINVER=0x0A00 \
      -mwindows -static -s -Wl,--no-insert-timestamp \
      -o "$exe" "$src" -ld3d11 -ldxgi -ld2d1 -ldwmapi -lgdi32 -luser32
  done'

[ -n "$tarball" ] || exit 0
[ -f "$tarball" ] || { echo "build.sh: no Wine source tarball at $tarball" >&2; exit 1; }
stamp=$out/winetests/built-from
sum=$(sha256sum "$tarball" | cut -d' ' -f1)
if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$sum" ]; then exit 0; fi
work=$out/wine-src
[ -n "$work" ] && [ "$work" != / ] || { echo "VML-88: work is empty or /" >&2; exit 1; }
rm -rf "$work" "$out/winetests"
mkdir -p "$work" "$out/winetests/x64"
top=$(basename "$tarball" .tar.xz)
tar -C "$work" --strip-components=1 -xf "$tarball" "$top/include" "$top/dlls/d3d9/tests" "$top/dlls/d3d10/tests" \
    "$top/dlls/d3d10core/tests" "$top/dlls/d3d10_1/tests" "$top/dlls/d3d11/tests" "$top/COPYING.LIB"
mkdir -p "$work/gen"
[ -n "$here" ] && [ -d "$here" ] || exit 1
cp "$here/wine/shim.c" "$here/wine/d3d10_1.def" "$work/gen/"

docker run --rm --user "$(id -u):$(id -g)" -v "$out:/out" -w /out/wine-src "$image" bash -c '
  set -e
  x86_64-w64-mingw32-clang -O1 -w -c -o gen/shim.o gen/shim.c
  x86_64-w64-mingw32-dlltool -d gen/d3d10_1.def -l gen/libd3d10_1.a
  # Headers mingw-w64 does not ship, from the IDL in the tarball.
  x86_64-w64-mingw32-widl -h -o gen/d3d9on12.h include/d3d9on12.idl
  (cd dlls/d3d11/tests && x86_64-w64-mingw32-windres -I . -o /out/wine-src/gen/d3d11_res.o resource.rc)
  libs_d3d9="-ld3d9 -luser32 -lgdi32"
  libs_d3d10="-ld3d10 -ld3dcompiler"
  libs_d3d10core="-ld3d10 -ldxgi -luser32"
  libs_d3d10_1="gen/libd3d10_1.a -luser32"
  libs_d3d11="-ld3d11 -ldxgi -luser32 -lgdi32 -ld3dcompiler -lntdll gen/d3d11_res.o"
  # reflection.c of d3d10 and d3d10_1 comes from d3dcompiler_43 and tests the
  # compiler, not the driver; it is left out.
  for f in dlls/d3d9/tests/d3d9ex.c dlls/d3d9/tests/device.c dlls/d3d9/tests/stateblock.c dlls/d3d9/tests/visual.c \
           dlls/d3d10/tests/device.c dlls/d3d10/tests/effect.c dlls/d3d10core/tests/d3d10core.c \
           dlls/d3d10_1/tests/d3d10_1.c dlls/d3d11/tests/d3d11.c; do
    module=$(echo "$f" | cut -d/ -f2)
    name=${module}_$(basename "$f" .c)
    eval "libs=\$libs_$module"
    # CURSOR_SUPPRESSED: a winuser.h constant this mingw-w64 does not have.
    # shellcheck disable=SC2086
    x86_64-w64-mingw32-clang -O1 -w -DSTANDALONE -D__WINESRC__ -DWINE_NO_LONG_TYPES -DD3D_COMPILER_VERSION=0 \
      -DDECLSPEC_EXPORT= -DCOBJMACROS -DCURSOR_SUPPRESSED=0x00000002 -include windows.h -I gen -idirafter include \
      -static -s -Wl,--no-insert-timestamp -o "/out/winetests/x64/$name.exe" "$f" gen/shim.o $libs -ldxguid -luuid
  done'
echo "$sum" >"$stamp"
