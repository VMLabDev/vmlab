#!/usr/bin/env bash
# Run vmlab with an isolated supervisor for historical diagnostic runs.
# Current labs use the installed vmlab-hv bundle. WIN3D_RENDER_SERVER and
# WIN3D_QEMU_ENV remain explicit diagnostic overrides.
set -euo pipefail
root=$HOME/win3d-zeus
export XDG_RUNTIME_DIR=$root/run
export XDG_STATE_HOME=$root/state
mkdir -p -m 700 "$XDG_RUNTIME_DIR"; mkdir -p "$XDG_STATE_HOME"
unset VK_ICD_FILENAMES VK_DRIVER_FILES VK_LOADER_DRIVERS_SELECT MESA_VK_DEVICE_SELECT RENDER_SERVER_EXEC_PATH
export PATH=$PATH:$HOME/.local/bin
[ -z "${WIN3D_RENDER_SERVER:-}" ] || export RENDER_SERVER_EXEC_PATH=$WIN3D_RENDER_SERVER
for kv in ${WIN3D_QEMU_ENV:-}; do export "${kv?}"; done
case ${1:-} in
    --state-home) echo "$XDG_STATE_HOME"; exit 0 ;;
    --which-qemu) echo "$(command -v qemu-system-x86_64) $(qemu-system-x86_64 --version | head -1)"; exit 0 ;;
    --stop-supervisor)
        if pgrep -f "vmlab __labd --lab win3d-zeus-" >/dev/null; then echo "a win3d-zeus lab daemon is still running; run down first"; exit 1; fi
        vmlab daemon stop; exit $? ;;
esac
exec vmlab "$@"
