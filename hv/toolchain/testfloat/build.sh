#!/bin/sh
# Build `testfloat_gen` of the pinned Berkeley TestFloat against the pinned
# upstream SoftFloat with the ARM-VFPv2 specialisation, in the directory $1.
# Prints the path of the generator. Needs curl, tar, make, gcc and ar.
set -eu
dir=${1:-}
case "$dir" in ""|/) echo "build.sh: give a directory" >&2; exit 2;; esac
here=$(cd "$(dirname "$0")" && pwd)
pin() { sed -n "s/^$1=//p" "$here/pins.env"; }
gen="$dir/berkeley-testfloat-3/build/Linux-x86_64-GCC/testfloat_gen"
stamp="$dir/built-$(pin TESTFLOAT_COMMIT)-$(pin SOFTFLOAT_COMMIT)"
if [ -x "$gen" ] && [ -f "$stamp" ]; then
    echo "$gen"
    exit 0
fi
mkdir -p "$dir"
fetch() {
    name=$1 commit=$2 sha=$3
    archive="$dir/$name.tar.gz"
    curl -sSfL -o "$archive" "https://github.com/ucb-bar/$name/archive/$commit.tar.gz"
    echo "$sha  $archive" | sha256sum -c - >&2
    tar -xzf "$archive" -C "$dir"
    # The archive unpacks into <name>-<commit>.
    [ -d "$dir/$name-$commit" ]
    if [ -e "$dir/$name" ]; then
        rm -rf -- "${dir:?}/$name"
    fi
    mv -- "$dir/$name-$commit" "$dir/$name"
}
fetch berkeley-softfloat-3 "$(pin SOFTFLOAT_COMMIT)" "$(pin SOFTFLOAT_SHA256)"
fetch berkeley-testfloat-3 "$(pin TESTFLOAT_COMMIT)" "$(pin TESTFLOAT_SHA256)"
# Upstream's makefile lists the bfloat16 objects, and the ARM-VFPv2
# specialisation has no bfloat16: those objects fail, every other one is
# built (-k), and the library is made from what was built. TestFloat is then
# built without bfloat16, so nothing asks for the missing objects.
softfloat="$dir/berkeley-softfloat-3/build/Linux-x86_64-GCC"
make -k -C "$softfloat" SPECIALIZE_TYPE=ARM-VFPv2 >"$dir/softfloat.log" 2>&1 || true
(cd "$softfloat" && ar crs softfloat.a ./*.o)
make -C "$dir/berkeley-testfloat-3/build/Linux-x86_64-GCC" \
    TESTFLOAT_OPTS="-DFLOAT16 -DFLOAT64 -DFLOAT_ROUND_ODD" testfloat_gen >"$dir/testfloat.log" 2>&1
[ -x "$gen" ]
: >"$stamp"
echo "$gen"
