#!/usr/bin/env bash
# Build the vmlab-agent guest binaries — the in-guest terminal/exec/file
# agent baked into templates (src/template/agent_install.rs) — one per
# guest target, under guest/dist/agent/<os>-<arch>/vmlab-agent[.exe] with a
# VERSION stamp.
#
# Targets:
#   linux-x86_64    x86_64-unknown-linux-musl   (static)
#   linux-aarch64   aarch64-unknown-linux-musl  (static, rust-lld cross)
#   linux-riscv64   riscv64gc-unknown-linux-musl (static; best-effort — the
#                   target is Tier 2 without host tools everywhere, skipped
#                   with a warning when not installed)
#   windows-x86_64  x86_64-pc-windows-gnu       (static CRT; needs mingw-w64,
#                   skipped with a warning when x86_64-w64-mingw32-gcc is
#                   absent)
#   windows-x86     i686-pc-windows-gnu         (static CRT; 32-bit Vista
#                   and later — older NT is the legacy tier, PRD §7.4; needs
#                   i686-w64-mingw32-gcc, skipped with a warning when absent)
#   linux-x86       i686-unknown-linux-musl     (static; optional — a 32-bit
#                   guest too old for virtio-serial serves on COM1 instead,
#                   PRD §7.4; skipped with a warning when not installed)
#
# Usage: guest/build-agent.sh [target-key...]   (default: all of the above)

set -euo pipefail

# The win7 targets need a nightly with rust-src; reuse the channel the
# ebpf workspace already pins so a checkout needs one nightly, not two.
NIGHTLY="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$(dirname "${BASH_SOURCE[0]}")/../ebpf/rust-toolchain.toml" 2>/dev/null || true)"
NIGHTLY="${NIGHTLY:-nightly}"
MSVCRT_PREFIX="${VMLAB_MSVCRT_PREFIX:-$HOME/.local/share/vmlab/toolchains/mingw-msvcrt}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$SCRIPT_DIR/dist/agent"

die() {
  echo "build-agent: error: $*" >&2
  exit 1
}

log() {
  echo "build-agent: $*" >&2
}

version_stamp() {
  local rev="unknown"
  if git -C "$SCRIPT_DIR" rev-parse --short HEAD >/dev/null 2>&1; then
    rev="$(git -C "$SCRIPT_DIR" rev-parse --short HEAD)"
    if [[ -n "$(git -C "$SCRIPT_DIR" status --porcelain -- "$SCRIPT_DIR/agent" "$SCRIPT_DIR/agent-proto" 2>/dev/null)" ]]; then
      rev="$rev-dirty"
    fi
  fi
  echo "agent=$rev"
}

# key -> "rust-target|binary-name|required(1)/optional(0)"
target_spec() {
  case "$1" in
    linux-x86_64) echo "x86_64-unknown-linux-musl|vmlab-agent|1" ;;
    linux-aarch64) echo "aarch64-unknown-linux-musl|vmlab-agent|1" ;;
    linux-riscv64) echo "riscv64gc-unknown-linux-musl|vmlab-agent|0" ;;
    windows-x86_64) echo "x86_64-win7-windows-gnu|vmlab-agent.exe|0" ;;
    windows-x86) echo "i686-win7-windows-gnu|vmlab-agent.exe|0" ;;
    linux-x86) echo "i686-unknown-linux-musl|vmlab-agent|0" ;;
    *) die "unknown target key '$1' (known: linux-x86_64 linux-aarch64 linux-riscv64 windows-x86_64 windows-x86 linux-x86)" ;;
  esac
}

# Link a Windows target against the msvcrt CRT from build-mingw-msvcrt.sh, if
# it is there. The distro toolchains are UCRT-only, and a UCRT import is a
# load-time dependency on Windows 10 — the agent will not start at all on
# Vista through Server 2012 R2 without this. Absent the prefix the build still
# succeeds; the binary just carries that floor, which the caller is warned of.
msvcrt_link_args() {
  local host="$1" lib="$MSVCRT_PREFIX/$1/lib"
  if [[ -f "$lib/libmsvcrt.a" && -f "$lib/crt2.o" ]]; then
    echo "-Clink-arg=-B$lib -Clink-arg=-L$lib"
  else
    log "no msvcrt CRT for $host — the binary will need the UCRT (Windows 10+);" \
        "build one with guest/build-mingw-msvcrt.sh"
  fi
}

build_one() {
  local key="$1" spec target binary required
  spec="$(target_spec "$key")"
  IFS='|' read -r target binary required <<<"$spec"

  # The *-win7-windows-gnu targets are tier 3: rustup ships no std for them, so
  # they are built from source with a nightly that has rust-src. They exist for
  # one reason — a std built for the Windows 7 API floor calls RtlGenRandom
  # instead of ProcessPrng, which is Windows 10 1809 and later. A binary that
  # names ProcessPrng does not load *at all* on anything older, agent and every
  # feature with it.
  local -a build_std=()
  local toolchain=""
  if [[ "$target" == *-win7-windows-* ]]; then
    if ! rustup toolchain list | grep -q "^$NIGHTLY"; then
      log "skipping $key: $NIGHTLY not installed (rustup toolchain install $NIGHTLY --component rust-src)"
      return 0
    fi
    if ! rustup component list --toolchain "$NIGHTLY" 2>/dev/null | grep -q "^rust-src (installed)"; then
      log "skipping $key: rust-src missing (rustup component add rust-src --toolchain $NIGHTLY)"
      return 0
    fi
    toolchain="+$NIGHTLY"
    build_std=(-Z build-std=std,panic_abort)
  elif ! rustup target list --installed | grep -qx "$target"; then
    if [[ "$required" == "1" ]]; then
      die "rust target $target not installed — run: rustup target add $target"
    fi
    log "skipping $key: rust target $target not installed (rustup target add $target)"
    return 0
  fi
  local mingw_cc=""
  case "$key" in
    windows-x86_64) mingw_cc="x86_64-w64-mingw32-gcc" ;;
    windows-x86) mingw_cc="i686-w64-mingw32-gcc" ;;
  esac
  if [[ -n "$mingw_cc" ]] && ! command -v "$mingw_cc" >/dev/null 2>&1; then
    log "skipping $key: $mingw_cc not found (install mingw-w64)"
    return 0
  fi

  local -a env_args=()
  case "$key" in
    linux-aarch64)
      env_args+=("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld")
      ;;
    linux-riscv64)
      # riscv64 musl does not default to +crt-static like the Tier-2
      # x86_64/aarch64 musl targets; force a static self-contained link.
      env_args+=(
        "CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_MUSL_LINKER=rust-lld"
        "CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_MUSL_RUSTFLAGS=-Ctarget-feature=+crt-static -Clink-self-contained=yes"
      )
      ;;
    windows-x86_64)
      env_args+=("CARGO_TARGET_X86_64_WIN7_WINDOWS_GNU_RUSTFLAGS=-Ctarget-feature=+crt-static $(msvcrt_link_args x86_64-w64-mingw32)")
      ;;
    windows-x86)
      env_args+=("CARGO_TARGET_I686_WIN7_WINDOWS_GNU_RUSTFLAGS=-Ctarget-feature=+crt-static $(msvcrt_link_args i686-w64-mingw32)")
      ;;
  esac

  log "building vmlab-agent for $key ($target)"
  env "${env_args[@]}" cargo $toolchain build --release --target "$target" "${build_std[@]}" \
    --manifest-path "$SCRIPT_DIR/agent/Cargo.toml" \
    || die "cargo build for $target failed"

  local out="$DIST_DIR/$key"
  mkdir -p "$out"
  install -m 0755 "$SCRIPT_DIR/agent/target/$target/release/$binary" "$out/$binary"
  version_stamp >"$out/VERSION"
  log "$key: $(du -h "$out/$binary" | cut -f1) → $out/$binary"
}

main() {
  command -v cargo >/dev/null 2>&1 || die "missing host tool: cargo"
  command -v rustup >/dev/null 2>&1 || die "missing host tool: rustup"
  mkdir -p "$DIST_DIR"
  local -a keys=("$@")
  [[ ${#keys[@]} -gt 0 ]] || keys=(linux-x86_64 linux-aarch64 linux-riscv64 windows-x86_64 windows-x86 linux-x86)
  local key
  for key in "${keys[@]}"; do
    build_one "$key"
  done
  log "done"
}

main "$@"
