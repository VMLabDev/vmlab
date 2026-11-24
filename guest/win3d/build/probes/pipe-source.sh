#!/usr/bin/env bash
# Writes the two copies of viogpu/shared/pipe.cpp, pipe.h and log.h of the
# kernel driver source that probes/pipe is linked with (card #757):
#
#   <dest>/before/   the pinned source with every patch of patches/kmd/series
#                    in front of kmd-read-helper-pipe-message-inside-buffer.patch
#   <dest>/after/    the same with that patch
#
# Usage: build/probes/pipe-source.sh <dest>
# <dest> must not exist or must be empty. The source is the tree
# build/kmd/build.sh unpacked into $WIN3D_CACHE (default .scratch/cache): run
# that first.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
dest=${1:?usage: pipe-source.sh <dest>}
cache=${WIN3D_CACHE:-$repo/.scratch/cache}
rev=$(awk '$1 == "yttrium-virtio-gpu" { print $4 }' "$repo/build/kmd/PINS")
tree=$cache/src/yttrium-virtio-gpu-$rev
[ -f "$tree/viogpu/shared/pipe.cpp" ] || { echo "no driver source at $tree: run build/kmd/build.sh" >&2; exit 1; }
patches=$repo/patches/kmd
fix=kmd-read-helper-pipe-message-inside-buffer.patch
grep -qx "$fix" "$patches/series" || { echo "$fix is not in the series" >&2; exit 1; }

mkdir -p "$dest"
[ -z "$(ls -A "$dest")" ] || { echo "pipe-source.sh: $dest is not empty" >&2; exit 1; }
for state in before after; do
    work=$dest/$state
    mkdir -p "$work/viogpu"
    [ -n "$work" ] && [ -d "$work" ] || exit 1
    cp -a "$tree/viogpu/shared" "$work/viogpu/shared"
    chmod -R u+w "$work"
    while IFS= read -r p; do
        [ "$p" != "$fix" ] || [ "$state" = after ] || break
        # Only the patches that touch the directory: the others have no file here.
        if grep -q '^diff --git a/viogpu/shared/' "$patches/$p"; then
            if grep '^diff --git ' "$patches/$p" | grep -qv '^diff --git a/viogpu/shared/'; then
                echo "$p changes viogpu/shared and other files: pipe-source.sh cannot apply it" >&2
                exit 1
            fi
            # The command of build/kmd/inner.sh.
            patch -d "$work" -p1 --no-backup-if-mismatch -s <"$patches/$p"
        fi
        [ "$p" != "$fix" ] || break
    done <"$patches/series"
done
[ ! -f "$dest/before/viogpu/shared/pipe_bounds.h" ] && [ -f "$dest/after/viogpu/shared/pipe_bounds.h" ] ||
    { echo "pipe-source.sh: the two copies are not the source before and after the patch" >&2; exit 1; }
echo "pipe-source: $dest/before and $dest/after from $rev"
