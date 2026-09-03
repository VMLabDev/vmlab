#!/usr/bin/env bash
# Build vmlab-agent-legacy — the C agent for guests that cannot drive
# virtio-serial (guest/agent-legacy, PRD §7.4) — one binary per guest
# target, under guest/dist/agent/<key>/ with a VERSION stamp, beside the
# Rust agent's outputs from build-agent.sh.
#
# Targets:
#   windows-nt-x86   NT4 through XP/2003 — mingw-w64 i686 (static CRT);
#                    skipped with a warning when i686-w64-mingw32-gcc is absent
#   windows-9x-x86   Windows 95/98/ME — OpenWatcom v2 (`win95` system);
#                    skipped with a warning when OpenWatcom is absent
#   dos-i386         DOS (32-bit, DOS/32A extender bound in) — OpenWatcom v2;
#                    skipped with a warning when OpenWatcom is absent
#   linux-x86        the POSIX build, host cc; also the conformance binary
#   templeos         guest/agent-templeos/VmlabAgt.HC, stamped — HolyC source is
#                    the artefact; TempleOS compiles it itself
#
# OpenWatcom is found through $WATCOM, else ~/.local/opt/open-watcom-v2
# (the unpacked ow-snapshot.tar.xz from the project's GitHub releases).
#
# Usage: guest/build-agent-legacy.sh [target-key...]   (default: all)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_DIR="$SCRIPT_DIR/agent-legacy/src"
DIST_DIR="$SCRIPT_DIR/dist/agent"
MSVCRT_PREFIX="${VMLAB_MSVCRT_PREFIX:-$HOME/.local/share/vmlab/toolchains/mingw-msvcrt}"
WATCOM="${WATCOM:-$HOME/.local/opt/open-watcom-v2}"

die() {
  echo "build-agent-legacy: error: $*" >&2
  exit 1
}

log() {
  echo "build-agent-legacy: $*" >&2
}

version_stamp() {
  local rev="unknown"
  if git -C "$SCRIPT_DIR" rev-parse --short HEAD >/dev/null 2>&1; then
    rev="$(git -C "$SCRIPT_DIR" rev-parse --short HEAD)"
    if [[ -n "$(git -C "$SCRIPT_DIR" status --porcelain -- "$SCRIPT_DIR/agent-legacy" 2>/dev/null)" ]]; then
      rev="$rev-dirty"
    fi
  fi
  echo "agent-legacy=$rev"
}

COMMON=(json.c wire.c agent.c)

have_watcom() {
  [[ -x "$WATCOM/binl64/wcc386" && -x "$WATCOM/binl64/wlink" ]]
}

# Compile with OpenWatcom for one `-bt` target and link with one wlink
# system. Objects go to a scratch dir; wcc386 has no combined compile+link
# with cross-target search paths worth relying on.
watcom_build() {
  local key="$1" bt="$2" system="$3" binary="$4" plat="$5" stamp="$6"
  local work
  work="$(mktemp -d)"
  local -a objs=()
  local f
  for f in "${COMMON[@]}" "$plat"; do
    local obj="$work/${f%.c}.obj"
    WATCOM="$WATCOM" PATH="$WATCOM/binl64:$WATCOM/binw:$PATH" \
      INCLUDE="$WATCOM/h:$WATCOM/h/nt" \
      "$WATCOM/binl64/wcc386" -q -zq -w4 -we -3r -os -bt="$bt" \
      -DAGENT_VERSION="\"$stamp\"" -fo="$obj" "$SRC_DIR/$f" \
      || die "wcc386 failed on $f for $key"
    objs+=("$obj")
  done
  local files
  files="$(IFS=,; echo "${objs[*]}")"
  local out="$DIST_DIR/$key"
  mkdir -p "$out"
  WATCOM="$WATCOM" PATH="$WATCOM/binl64:$WATCOM/binw:$PATH" \
    "$WATCOM/binl64/wlink" system "$system" option quiet name "$out/$binary" file "$files" \
    || die "wlink failed for $key"
  rm -rf "$work"
  echo "$stamp" >"$out/VERSION"
  log "$key: $(du -h "$out/$binary" | cut -f1) → $out/$binary"
}

build_one() {
  local key="$1"
  local stamp
  stamp="$(version_stamp)"
  local srcs=()
  local f
  for f in "${COMMON[@]}"; do srcs+=("$SRC_DIR/$f"); done
  case "$key" in
    windows-nt-x86)
      if ! command -v i686-w64-mingw32-gcc >/dev/null 2>&1; then
        log "skipping $key: i686-w64-mingw32-gcc not found (install mingw-w64)"
        return 0
      fi
      local out="$DIST_DIR/$key"
      mkdir -p "$out"
      # Against the msvcrt CRT when one is built, because the distro mingw is
      # UCRT-only and a UCRT import will not load on the very guests this
      # binary exists for — NT4 through 2003 shipped long before it. The
      # binary builds either way; without the prefix it silently inherits a
      # Windows 10 floor, which for the legacy tier is no use at all.
      local msvcrt_lib="$MSVCRT_PREFIX/i686-w64-mingw32/lib"
      local -a crt_args=()
      if [[ -f "$msvcrt_lib/libmsvcrt.a" && -f "$msvcrt_lib/crt2.o" ]]; then
        crt_args=("-B$msvcrt_lib" "-L$msvcrt_lib")
      else
        log "$key: no msvcrt CRT — this binary will need the UCRT (Windows 10+);" \
            "build one with guest/build-mingw-msvcrt.sh"
      fi
      i686-w64-mingw32-gcc -std=c99 -Wall -Wextra -Werror -Wno-cast-function-type -Os -s \
        "${crt_args[@]}" -static -DAGENT_VERSION="\"$stamp\"" \
        -o "$out/vmlab-agent-legacy.exe" "${srcs[@]}" "$SRC_DIR/plat_win32.c" \
        -ladvapi32 -luser32 \
        || die "mingw build failed for $key"
      echo "$stamp" >"$out/VERSION"
      log "$key: $(du -h "$out/vmlab-agent-legacy.exe" | cut -f1) → $out/vmlab-agent-legacy.exe"
      ;;
    windows-9x-x86)
      if ! have_watcom; then
        log "skipping $key: OpenWatcom not found at $WATCOM (set WATCOM)"
        return 0
      fi
      watcom_build "$key" nt win95 vmlab-agent-legacy.exe plat_win32.c "$stamp"
      ;;
    dos-i386)
      if ! have_watcom; then
        log "skipping $key: OpenWatcom not found at $WATCOM (set WATCOM)"
        return 0
      fi
      # 8.3, and the name the DOS install notes refer to.
      watcom_build "$key" dos dos32a VMLABAGT.EXE plat_dos.c "$stamp"
      ;;
    linux-x86)
      command -v cc >/dev/null 2>&1 || die "missing host tool: cc"
      local out="$DIST_DIR/$key"
      mkdir -p "$out"
      cc -std=c99 -Wall -Wextra -Werror -Os -s -DAGENT_VERSION="\"$stamp\"" \
        -o "$out/vmlab-agent-legacy" "${srcs[@]}" "$SRC_DIR/plat_posix.c" \
        || die "cc build failed for $key"
      echo "$stamp" >"$out/VERSION"
      log "$key: $(du -h "$out/vmlab-agent-legacy" | cut -f1) → $out/vmlab-agent-legacy"
      ;;
    templeos)
      local out="$DIST_DIR/templeos"
      # The stamp names the flavour: verify tells agents apart by it.
      stamp="${stamp/agent-legacy=/agent-templeos=}"
      mkdir -p "$out"
      sed "s/^#define VA_VERSION .*/#define VA_VERSION  \"$stamp\"/" \
        "$SCRIPT_DIR/agent-templeos/VmlabAgt.HC" >"$out/VmlabAgt.HC"
      grep -q "\"$stamp\"" "$out/VmlabAgt.HC" || die "VA_VERSION stamp not applied"
      echo "$stamp" >"$out/VERSION"
      log "$key: $(du -h "$out/VmlabAgt.HC" | cut -f1) → $out/VmlabAgt.HC"
      ;;
    *) die "unknown target key '$key' (known: windows-nt-x86 windows-9x-x86 dos-i386 linux-x86 templeos)" ;;
  esac
}

main() {
  mkdir -p "$DIST_DIR"
  local -a keys=("$@")
  [[ ${#keys[@]} -gt 0 ]] || keys=(windows-nt-x86 windows-9x-x86 dos-i386 linux-x86 templeos)
  local key
  for key in "${keys[@]}"; do
    build_one "$key"
  done
  log "done"
}

main "$@"
