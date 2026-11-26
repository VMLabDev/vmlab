#!/bin/sh
# Dump the AArch64 TableGen records as JSON into the directory $1.
# Reads LLVM_SRC (the root of llvm-project at the pinned commit) and
# TD_SHA256 (the list of pinned .td files, default /opt/td.sha256).
# With PIN=1 it writes the list in place of checking it.
set -eu
out=${1:-}
case "$out" in ""|/) echo "dump.sh: give an output directory" >&2; exit 2;; esac
src=${LLVM_SRC:-}
case "$src" in ""|/) echo "dump.sh: LLVM_SRC is not set" >&2; exit 2;; esac
list=${TD_SHA256:-/opt/td.sha256}
tblgen=${LLVM_TBLGEN:-llvm-tblgen}
mkdir -p "$out"
[ -n "$out" ] && [ -d "$out" ] || exit 1
target=llvm/lib/Target/AArch64
( cd "$src" && "$tblgen" --dump-json -I llvm/include -I "$target" \
    "$target/AArch64.td" -d "$out/aarch64.d" -o "$out/aarch64.json" )
# The depfile names every file that tblgen read.
read_files=$(sed -e 's/^[^:]*://' -e 's/\\$//' "$out/aarch64.d" | tr ' ' '\n' | grep '\.td$' | sort -u)
sums=$(cd "$src" && sha256sum $read_files)
if [ "${PIN:-0}" = 1 ]; then
    printf '%s\n' "$sums" > "$list"
    echo "dump.sh: pinned $(printf '%s\n' "$sums" | wc -l) .td files in $list"
else
    printf '%s\n' "$sums" | cmp -s - "$list" || {
        echo "dump.sh: the .td files that tblgen read differ from $list" >&2
        printf '%s\n' "$sums" | diff - "$list" >&2 || true
        exit 1
    }
fi
"$tblgen" --version | sed -n 's/^ *LLVM version /llvm-tblgen /p' > "$out/aarch64.versions.txt"
( cd "$out" && sha256sum aarch64.json > aarch64.json.sha256 )
rm -f "$out/aarch64.d"
