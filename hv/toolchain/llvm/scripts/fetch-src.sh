#!/bin/sh
# Fetch the part of llvm-project at the pinned commit that the JSON dump
# reads, into the directory $1. For a host that has llvm-tblgen and llvm-mc
# of the pinned version and does not want to build the container.
set -eu
dir=${1:-}
case "$dir" in ""|/) echo "fetch-src.sh: give a directory" >&2; exit 2;; esac
here=$(cd "$(dirname "$0")/.." && pwd)
commit=$(sed -n 's/^LLVM_COMMIT=//p' "$here/pins.env")
if [ -d "$dir/.git" ] && [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$commit" ]; then
    exit 0
fi
mkdir -p "$dir"
git init -q "$dir"
git -C "$dir" remote add origin https://github.com/llvm/llvm-project.git 2>/dev/null || true
git -C "$dir" sparse-checkout set llvm/include/llvm llvm/lib/Target/AArch64
git -C "$dir" fetch -q --depth 1 --filter=blob:none origin "$commit"
git -C "$dir" checkout -q FETCH_HEAD
test "$(git -C "$dir" rev-parse HEAD)" = "$commit"
