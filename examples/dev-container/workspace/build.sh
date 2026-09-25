#!/bin/sh
# The "build" step of the worked example, run from a shell in the guest:
#
#   cd /src && ./build.sh
#
# Compile-free on purpose: what is being demonstrated is the login and the
# workspace, not a toolchain.
set -eu
echo "running as $(id -un) in $(pwd), home $HOME"
echo "built by $(id -un)" > ./out.txt
echo "wrote /src/out.txt; it appears in ./workspace on the host within a second"
