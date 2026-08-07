#!/bin/sh
# The "build" step of the worked example, run from the attached shell:
#
#   cd /src && ./build.sh
#
# Compile-free on purpose: what is being demonstrated is the session and the
# workspace, not a toolchain.
set -eu
echo "running as $(id -un) in $(pwd)"
nvim --headless -c 'lua print(vim.inspect(vim.api.nvim_list_runtime_paths()))' -c q 2>&1 | head -20
echo "built by $(id -un)" > ./out.txt
echo "wrote /src/out.txt — it appears in ./workspace on the host within a second"
