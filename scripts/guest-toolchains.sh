#!/usr/bin/env bash
# Install every toolchain `just guest-package` needs on a stock Ubuntu 24.04
# (what GitHub's ubuntu-latest is), so the strict build skips nothing — and,
# as the opt-in `ebpf` stage, the bpf-linker the BPF objects build with.
#
# The release workflow's guest-assets job runs this, and so does the
# from-scratch check in a bare `ubuntu:24.04` container, and build/Dockerfile
# (one stage per image layer) — one definition of what the bundle needs, not a
# YAML copy that drifts from it.
#
# Usage: scripts/guest-toolchains.sh [stage...]
#   apt     host packages: cc + gcc-multilib (linux-x86 legacy, i686 musl
#           link), mingw-w64 (i686 + x86_64), cpio/xz/curl/git/bzip2/make
#   rust    rustup's pinned stable (rust-toolchain.toml) + the four musl
#           targets (rust-lld rides along with that toolchain) and
#           x86_64-apple-darwin, and the nightly ebpf/rust-toolchain.toml
#           pins, with rust-src, for the win7 targets' build-std
#   zig     the macOS agent's linker and signer, all pinned below: zig into
#           $ZIG_DIR (default ~/.local/opt/zig), cargo-zigbuild into cargo's
#           bin, rcodesign into $VMLAB_BIN_DIR; each skipped when present
#   msvcrt  the msvcrt-flavoured mingw CRT (guest/build-mingw-msvcrt.sh) at
#           the version of the installed mingw-w64 headers; skipped when
#           $VMLAB_MSVCRT_PREFIX already holds it (a CI cache hit)
#   watcom  OpenWatcom v2, the pinned snapshot below, unpacked into $WATCOM
#           (default ~/.local/opt/open-watcom-v2); skipped when present
#   just    the `just` runner, into $VMLAB_BIN_DIR (default ~/.local/bin),
#           when not already on PATH
#   ebpf    bpf-linker, built against the nightly ebpf/rust-toolchain.toml
#           pins (`just ebpf-tools`) — not a bundle toolchain, so not in the
#           default set; CI's ebpf-verify job and build/Dockerfile run it
# No stage given: the first six, in that order.
#
# Runs as root or as a user with passwordless sudo (apt only).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# OpenWatcom v2 publishes a rolling "Current-build" plus monthly tags; pin a
# monthly one and its digest so a release is built by a known compiler.
OW_TAG="2026-09-01-Build"
OW_SHA256="bac354f3c75ffa49ff8d70a44e475de7e7c1823fff04b80c14787bd0792c9bdf"
OW_URL="https://github.com/open-watcom/open-watcom-v2/releases/download/$OW_TAG/ow-snapshot.tar.xz"

# bpf-linker links against the pinned nightly's libLLVM through its proxy, so
# the objects are reproducible given this version and the channel pin.
BPF_LINKER_VERSION="0.10.3"

# The macOS agent (x86_64-apple-darwin) links through zig, which carries the
# libSystem stubs an Apple SDK would otherwise provide, and is ad-hoc signed
# by rcodesign. cargo-zigbuild 0.24 is the release built against zig 0.16.
ZIG_VERSION="0.16.0"
ZIG_SHA256="70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00"
ZIG_URL="https://ziglang.org/download/$ZIG_VERSION/zig-x86_64-linux-$ZIG_VERSION.tar.xz"
CARGO_ZIGBUILD_VERSION="0.24.0"
RCODESIGN_VERSION="0.29.0"
RCODESIGN_SHA256="dbe85cedd8ee4217b64e9a0e4c2aef92ab8bcaaa41f20bde99781ff02e600002"
RCODESIGN_URL="https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F$RCODESIGN_VERSION/apple-codesign-$RCODESIGN_VERSION-x86_64-unknown-linux-musl.tar.gz"

WATCOM="${WATCOM:-$HOME/.local/opt/open-watcom-v2}"
ZIG_DIR="${ZIG_DIR:-$HOME/.local/opt/zig}"
MSVCRT_PREFIX="${VMLAB_MSVCRT_PREFIX:-$HOME/.local/share/vmlab/toolchains/mingw-msvcrt}"

log() { echo "guest-toolchains: $*" >&2; }
die() { echo "guest-toolchains: error: $*" >&2; exit 1; }

as_root() {
  if [[ "$(id -u)" == 0 ]]; then "$@"; else sudo "$@"; fi
}

stage_apt() {
  log "apt: host packages"
  as_root env DEBIAN_FRONTEND=noninteractive apt-get update -q
  as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends \
    ca-certificates curl git xz-utils bzip2 cpio gzip make file \
    build-essential gcc-multilib \
    gcc-mingw-w64-i686 gcc-mingw-w64-x86-64 binutils-mingw-w64 mingw-w64-common
}

# The stable toolchain rust-toolchain.toml pins for the host and guest crates.
pinned_stable() {
  local stable
  stable="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$ROOT/rust-toolchain.toml")"
  [[ -n "$stable" ]] || die "no channel in rust-toolchain.toml"
  echo "$stable"
}

# The nightly ebpf/rust-toolchain.toml pins: the BPF objects build with it,
# and the win7 agent targets reuse it for build-std.
pinned_nightly() {
  local nightly
  nightly="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$ROOT/ebpf/rust-toolchain.toml")"
  [[ -n "$nightly" ]] || die "no channel in ebpf/rust-toolchain.toml"
  echo "$nightly"
}

stage_rust() {
  if ! command -v rustup >/dev/null 2>&1; then
    log "rust: installing rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
      sh -s -- -y --profile minimal --default-toolchain none
  fi
  # shellcheck disable=SC1091
  [[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
  local stable
  stable="$(pinned_stable)"
  log "rust: $stable + musl targets"
  rustup toolchain install "$stable" --profile minimal
  rustup default "$stable"
  rustup target add --toolchain "$stable" \
    x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    riscv64gc-unknown-linux-musl i686-unknown-linux-musl \
    x86_64-apple-darwin
  local nightly
  nightly="$(pinned_nightly)"
  log "rust: $nightly + rust-src (win7 targets)"
  rustup toolchain install "$nightly" --profile minimal --component rust-src
}

stage_ebpf() {
  command -v rustup >/dev/null 2>&1 || die "ebpf: rustup is not installed (run the rust stage)"
  local nightly
  nightly="$(pinned_nightly)"
  log "ebpf: $nightly + rust-src, bpf-linker $BPF_LINKER_VERSION against it"
  rustup toolchain install "$nightly" --profile minimal --component rust-src
  # --force: rebuild even when a bpf-linker is present, since one built
  # against another toolchain would dlopen the wrong libLLVM.
  (cd "$ROOT/ebpf" && rustup run "$nightly" \
    cargo install bpf-linker --version "$BPF_LINKER_VERSION" --locked --force)
}

# The installed mingw-w64 headers' upstream version (Debian: 11.0.1-3build1
# → 11.0.1). The CRT is built to match them, not build-mingw-msvcrt.sh's
# default, so headers and import libraries agree.
mingw_version() {
  dpkg-query -W -f='${Version}' mingw-w64-common 2>/dev/null | sed 's/^[0-9]*://; s/-.*//'
}

stage_msvcrt() {
  local have=1 host
  for host in i686-w64-mingw32 x86_64-w64-mingw32; do
    [[ -f "$MSVCRT_PREFIX/$host/lib/libmsvcrt.a" && -f "$MSVCRT_PREFIX/$host/lib/crt2.o" ]] || have=0
  done
  if [[ "$have" == 1 ]]; then
    log "msvcrt: present under $MSVCRT_PREFIX"
    return
  fi
  local ver
  ver="$(mingw_version)"
  [[ -n "$ver" ]] || die "msvcrt: mingw-w64-common is not installed (run the apt stage)"
  log "msvcrt: building mingw-w64 $ver CRT against msvcrt"
  VMLAB_MSVCRT_PREFIX="$MSVCRT_PREFIX" "$ROOT/guest/build-mingw-msvcrt.sh" "$ver"
}

stage_watcom() {
  if [[ -x "$WATCOM/binl64/wcc386" && -x "$WATCOM/binl64/wlink" ]]; then
    log "watcom: present at $WATCOM"
    return
  fi
  log "watcom: fetching OpenWatcom v2 $OW_TAG"
  local tmp
  tmp="$(mktemp -d)"
  curl -fSL --retry 3 -o "$tmp/ow.tar.xz" "$OW_URL" || die "download failed: $OW_URL"
  echo "$OW_SHA256  $tmp/ow.tar.xz" | sha256sum -c --quiet - ||
    die "OpenWatcom snapshot digest mismatch ($OW_URL)"
  rm -rf "$WATCOM"
  mkdir -p "$WATCOM"
  tar -xJf "$tmp/ow.tar.xz" -C "$WATCOM"
  rm -rf "$tmp"
  [[ -x "$WATCOM/binl64/wcc386" ]] || die "watcom: no binl64/wcc386 in the snapshot"
  log "watcom: unpacked into $WATCOM"
}

# Fetch $1 to $2 and check it against the sha256 $3.
fetch_pinned() {
  curl -fSL --retry 3 -o "$2" "$1" || die "download failed: $1"
  echo "$3  $2" | sha256sum -c --quiet - || die "digest mismatch: $1"
}

stage_zig() {
  local bin="${VMLAB_BIN_DIR:-$HOME/.local/bin}" tmp
  mkdir -p "$bin"
  if [[ -x "$ZIG_DIR/zig" ]] && [[ "$("$ZIG_DIR/zig" version)" == "$ZIG_VERSION" ]]; then
    log "zig: $ZIG_VERSION present at $ZIG_DIR"
  else
    log "zig: fetching $ZIG_VERSION"
    tmp="$(mktemp -d)"
    fetch_pinned "$ZIG_URL" "$tmp/zig.tar.xz" "$ZIG_SHA256"
    rm -rf "$ZIG_DIR"
    mkdir -p "$ZIG_DIR"
    tar -xJf "$tmp/zig.tar.xz" -C "$ZIG_DIR" --strip-components=1
    rm -rf "$tmp"
  fi
  ln -sf "$ZIG_DIR/zig" "$bin/zig"
  if command -v cargo-zigbuild >/dev/null 2>&1 &&
    [[ "$(cargo-zigbuild --version)" == "cargo-zigbuild $CARGO_ZIGBUILD_VERSION" ]]; then
    log "zig: cargo-zigbuild $CARGO_ZIGBUILD_VERSION already installed"
  else
    log "zig: installing cargo-zigbuild $CARGO_ZIGBUILD_VERSION"
    cargo install cargo-zigbuild --version "$CARGO_ZIGBUILD_VERSION" --locked --force
  fi
  if [[ -x "$bin/rcodesign" ]] && "$bin/rcodesign" --version | grep -q " $RCODESIGN_VERSION"; then
    log "zig: rcodesign $RCODESIGN_VERSION present in $bin"
  else
    log "zig: fetching rcodesign $RCODESIGN_VERSION"
    tmp="$(mktemp -d)"
    fetch_pinned "$RCODESIGN_URL" "$tmp/rcodesign.tar.gz" "$RCODESIGN_SHA256"
    tar -xzf "$tmp/rcodesign.tar.gz" -C "$tmp" --strip-components=1
    install -m 0755 "$tmp/rcodesign" "$bin/rcodesign"
    rm -rf "$tmp"
  fi
}

stage_just() {
  if command -v just >/dev/null 2>&1; then
    log "just: $(just --version) already on PATH"
    return
  fi
  local bin="${VMLAB_BIN_DIR:-$HOME/.local/bin}"
  log "just: installing into $bin"
  mkdir -p "$bin"
  curl --proto '=https' --tlsv1.2 -sSf https://just.systems/install.sh |
    bash -s -- --to "$bin"
}

main() {
  local -a stages=("$@")
  [[ ${#stages[@]} -gt 0 ]] || stages=(apt rust zig msvcrt watcom just)
  local s
  for s in "${stages[@]}"; do
    case "$s" in
      apt | rust | zig | msvcrt | watcom | just | ebpf) "stage_$s" ;;
      *) die "unknown stage '$s' (known: apt rust zig msvcrt watcom just ebpf)" ;;
    esac
  done
}

main "$@"
