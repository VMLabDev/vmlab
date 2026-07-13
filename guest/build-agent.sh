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
#
# Usage: guest/build-agent.sh [target-key...]   (default: all of the above)

set -euo pipefail

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
    windows-x86_64) echo "x86_64-pc-windows-gnu|vmlab-agent.exe|0" ;;
    *) die "unknown target key '$1' (known: linux-x86_64 linux-aarch64 linux-riscv64 windows-x86_64)" ;;
  esac
}

build_one() {
  local key="$1" spec target binary required
  spec="$(target_spec "$key")"
  IFS='|' read -r target binary required <<<"$spec"

  if ! rustup target list --installed | grep -qx "$target"; then
    if [[ "$required" == "1" ]]; then
      die "rust target $target not installed — run: rustup target add $target"
    fi
    log "skipping $key: rust target $target not installed (rustup target add $target)"
    return 0
  fi
  if [[ "$key" == windows-* ]] && ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    log "skipping $key: x86_64-w64-mingw32-gcc not found (install mingw-w64)"
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
      env_args+=("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS=-Ctarget-feature=+crt-static")
      ;;
  esac

  log "building vmlab-agent for $key ($target)"
  env "${env_args[@]}" cargo build --release --target "$target" \
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
  [[ ${#keys[@]} -gt 0 ]] || keys=(linux-x86_64 linux-aarch64 linux-riscv64 windows-x86_64)
  local key
  for key in "${keys[@]}"; do
    build_one "$key"
  done
  log "done"
}

main "$@"
