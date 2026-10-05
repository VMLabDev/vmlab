#!/usr/bin/env bash
# Build every release artefact into target/dist/. Runs inside the vmlab-build
# container (`just buildbox::dist`); on a host with the same toolchains it runs
# as well.
#
#   vmlab-<version>-linux-x86_64              the release CLI, named as CI names
#                                             the release asset
#   vmlab-guest-<version>.tar.gz (+ .sha256)  the strict guest bundle, from
#                                             `just guest-package`
#   bpf/{fastpath_sockmap,xdp_switch}.bpf.o   the BPF objects rebuilt from
#                                             ebpf/ by `just ebpf-build`
#
# Tracked files are left exactly as they were. A version other than
# Cargo.toml's is stamped into Cargo.toml (and so Cargo.lock) for the binary
# build only, as CI's build job does, and put back afterwards. The BPF rebuild
# writes over the committed objects, so the working copies are saved first and
# restored after; a rebuilt object that differs from the committed one is
# reported, and fails the build (`just ci::ebpf-verify` would reject it too).
#
# Usage: build/dist.sh [version]     (default: the version in Cargo.toml)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
OUT="$ROOT/target/dist"
BPF_OBJS=(fastpath_sockmap.bpf.o xdp_switch.bpf.o)
BPF_DIR=src/net/fastpath/bpf

log() { echo "dist: $*" >&2; }
die() {
  echo "dist: error: $*" >&2
  exit 1
}

cargo_version() { sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1; }

SAVE="$(mktemp -d)"
restore() {
  local f
  for f in Cargo.toml Cargo.lock "${BPF_OBJS[@]}"; do
    [[ -f "$SAVE/$f" ]] || continue
    case "$f" in
      *.bpf.o) cp -p "$SAVE/$f" "$BPF_DIR/$f" ;;
      *) cp -p "$SAVE/$f" "$f" ;;
    esac
  done
  rm -rf "$SAVE"
}
trap restore EXIT

build_binary() {
  local version="$1" locked=(--locked)
  if [[ "$version" != "$(cargo_version)" ]]; then
    log "stamping $version into Cargo.toml for the binary build (restored after)"
    cp -p Cargo.toml Cargo.lock "$SAVE/"
    sed -i '0,/^version = ".*"$/s//version = "'"$version"'"/' Cargo.toml
    # The lock carries the package version, so a stamped build cannot be --locked.
    locked=()
  fi
  log "building vmlab $version (release)"
  cargo build --release "${locked[@]}" --bin vmlab
  install -m 0755 target/release/vmlab "$OUT/vmlab-$version-linux-x86_64"
  if [[ -f "$SAVE/Cargo.toml" ]]; then
    cp -p "$SAVE/Cargo.toml" Cargo.toml
    cp -p "$SAVE/Cargo.lock" Cargo.lock
    rm "$SAVE/Cargo.toml" "$SAVE/Cargo.lock"
  fi
}

build_bundle() {
  local version="$1" name="vmlab-guest-$1.tar.gz"
  just guest-package "$version"
  cp "target/guest-package/$name" "target/guest-package/$name.sha256" "$OUT/"
}

# 0 when every rebuilt object matches the committed one.
build_bpf() {
  local f differ=()
  # -p both ways: the binary include_bytes!()s these, so a restored copy with
  # a fresh mtime would make the next build recompile vmlab for nothing.
  for f in "${BPF_OBJS[@]}"; do cp -p "$BPF_DIR/$f" "$SAVE/$f"; done
  just ebpf-build
  mkdir -p "$OUT/bpf"
  for f in "${BPF_OBJS[@]}"; do
    cp "$BPF_DIR/$f" "$OUT/bpf/$f"
    git show "HEAD:$BPF_DIR/$f" | cmp -s - "$OUT/bpf/$f" || differ+=("$f")
  done
  for f in "${BPF_OBJS[@]}"; do cp -p "$SAVE/$f" "$BPF_DIR/$f" && rm "$SAVE/$f"; done
  if [[ ${#differ[@]} -gt 0 ]]; then
    log "the rebuilt BPF objects differ from the committed ones: ${differ[*]}"
    log "  rebuilt copies: $OUT/bpf/ — $BPF_DIR/ is untouched; run \`just ebpf-build\` to update it"
    return 1
  fi
  log "BPF objects match the committed ones"
}

main() {
  local version="${1:-}"
  [[ -n "$version" ]] || version="$(cargo_version)"
  [[ -n "$version" ]] || die "no version given and none found in Cargo.toml"
  version="${version#v}"

  rm -rf "$OUT"
  mkdir -p "$OUT"
  build_binary "$version"
  build_bundle "$version"
  local bpf_ok=0
  build_bpf || bpf_ok=1

  log "artefacts in $OUT:"
  (cd "$OUT" && find . -type f -printf '%P\t%s bytes\n' | sort) >&2
  [[ "$bpf_ok" == 0 ]] || die "BPF objects differ from the committed ones (see above)"
}

main "$@"
