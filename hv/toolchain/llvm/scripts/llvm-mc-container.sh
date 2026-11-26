#!/bin/sh
# Run the llvm-mc of the pinned LLVM container image, with stdin and stdout
# passed through. The decoder check starts this in place of a host llvm-mc.
set -eu
image=${A64_LLVM_IMAGE:?set A64_LLVM_IMAGE to the tag of the toolchain image}
exec docker run --rm -i --network none --memory 4g --memory-swap 4g "$image" llvm-mc "$@"
