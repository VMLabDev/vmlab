#!/usr/bin/env bash
# The tearing probe of card #134: count torn frames on a VM's console while a
# WPF window animates in the guest.
#
#   tests/tearing/run.sh [--seconds N] [--frames N] [--load PERCENT --cpus LIST]
#                        [--full] [--trace] [--out DIR] <lab-dir> <vm>
#
# <lab-dir> holds the vmlab.wcl of a running lab whose <vm> has the package
# installed and a user logged on at the console (tests/e2e/run.sh --keep
# leaves such a lab). The script copies tests/tearing and tests/guest into
# the guest, starts Start-TearWindow.ps1 in the desktop session, and runs
# rfbgrab.py on the VM's vnc.sock for --seconds (default 60) or until
# --frames frames showed the window.
#
# --load PERCENT --cpus LIST is the slow host of the issue: the VM's
# vmlab-hv and its render server are pinned to the CPUs of LIST (as 8-15 or
# 8,9,10), the render server is put at SCHED_IDLE, and hog.py keeps those
# CPUs busy for PERCENT of the time. The render server stays at SCHED_IDLE
# afterwards: an unprivileged user cannot raise it again. Restart the VM to
# undo it.
#
# --full makes the window change as a whole on every frame, so that each
# present copies all of it. That is the hardest case for the present copy.
#
# --trace also reads the flip trace of a trace-build kernel driver
# (build/kmd/README.md) for the first 20 s into <out>/fliptrace.csv.
#
# Prints rfbgrab.py's JSON, and writes it with the guest's report and the
# first torn frames to --out (default .scratch/tearing/<vm>). Needs uv.
# Exit status: 0 no torn frame, 1 torn frames, 2 usage or the window was
# never seen.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
VMLAB=${VMLAB:-vmlab}
SECONDS_RUN=60; FRAMES=0; LOAD=0; CPUS=; TRACE=0; OUT=; FULL=
usage() { sed -n '2,32p' "$0"; }
while [ $# -gt 0 ]; do
    case $1 in
        --seconds) SECONDS_RUN=${2:-}; shift 2 ;;
        --frames) FRAMES=${2:-}; shift 2 ;;
        --load) LOAD=${2:-}; shift 2 ;;
        --cpus) CPUS=${2:-}; shift 2 ;;
        --trace) TRACE=1; shift ;;
        --full) FULL=",'-Full'"; shift ;;
        --out) OUT=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) usage >&2; exit 2 ;;
        *) break ;;
    esac
done
[ $# -eq 2 ] || { usage >&2; exit 2; }
[[ $SECONDS_RUN =~ ^[0-9]+$ && $FRAMES =~ ^[0-9]+$ && $LOAD =~ ^[0-9]+$ ]] || { usage >&2; exit 2; }
if [ "$LOAD" != 0 ]; then [[ $CPUS =~ ^[0-9]+([-,][0-9]+)*$ ]] || { echo "--load needs --cpus" >&2; exit 2; }; fi
LABDIR=$(cd "$1" && pwd); VM=$2
OUT=${OUT:-$REPO/.scratch/tearing/$VM}
[ -n "$OUT" ] && [ "$OUT" != / ] || { echo "VML-88: OUT is empty or /" >&2; exit 2; }
mkdir -p "$OUT"
# The VM's vmlab-hv is a child of the lab daemon of this directory. Its
# --machine argument names the runtime directory that holds vnc.sock.
LABD=$(pgrep -u "$(id -u)" -f "__labd .*--root $LABDIR( |\$)" | head -1 || true)
[ -n "$LABD" ] || { echo "no lab daemon for $LABDIR: is the lab up?" >&2; exit 2; }
HV=; SOCK=
for p in $(pgrep -P "$LABD" || true); do
    m=$(tr '\0' '\n' <"/proc/$p/cmdline" | grep -A1 -x -e --machine | tail -1 || true)
    case $m in */vms/"$VM"/machine.json) HV=$p; SOCK=$(dirname "$m")/vnc.sock ;; esac
done
[ -n "$HV" ] && [ -S "$SOCK" ] || { echo "no vmlab-hv with an RFB socket for $VM in $LABDIR" >&2; exit 2; }

in_lab() { (cd "$LABDIR" && "$@"); }
gfile() { local t=$1; shift; in_lab "$VMLAB" exec --timeout "$t" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$@" | tr -d '\r'; }

in_lab "$VMLAB" cp "$HERE" "$VM:C:/win3d/tearing"
in_lab "$VMLAB" cp "$REPO/tests/guest" "$VM:C:/win3d/guest"

HOG=; WINDOW=; TRACER=
cleanup() {
    [ -z "$HOG" ] || kill "$HOG" 2>/dev/null || true
    [ -z "$WINDOW" ] || kill "$WINDOW" 2>/dev/null || true
    [ -z "$TRACER" ] || kill "$TRACER" 2>/dev/null || true
}
trap cleanup EXIT

# The window outlives the grab by a margin, so the grab never sees it close.
in_lab "$VMLAB" exec --timeout $((SECONDS_RUN + 400)) "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \
    "& 'C:\\win3d\\guest\\Run-Probe.ps1' -Path 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe' -Raw -NoModules -Name tearwindow -TimeoutSec $((SECONDS_RUN + 300)) -OutDir 'C:\\win3d\\results' -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','C:\\win3d\\tearing\\Start-TearWindow.ps1','-Seconds','$((SECONDS_RUN + 60))'$FULL" \
    >"$OUT/window-run.json" 2>&1 &
WINDOW=$!
sleep 20

if [ "$LOAD" != 0 ]; then
    # The render server and its per-context workers are descendants of vmlab-hv.
    tree() { local p; echo "$1"; for p in $(pgrep -P "$1" || true); do tree "$p"; done; }
    PIDS=$(tree "$HV")
    for p in $PIDS; do taskset -a -c -p "$CPUS" "$p" >/dev/null; done
    for p in $PIDS; do
        [ "$p" = "$HV" ] || chrt -a -i -p 0 "$p"
    done
    {
        echo "load: $LOAD percent on CPUs $CPUS"
        for p in $PIDS; do printf '%s %s %s\n' "$p" "$(cat /proc/"$p"/comm)" "$(chrt -p "$p" | head -1)"; done
    } >"$OUT/load.txt"
    LIST=$(python3 -c 'import sys; print(",".join(str(c) for p in sys.argv[1].split(",") for c in (range(int(p.split("-")[0]), int(p.split("-")[-1]) + 1))))' "$CPUS")
    python3 "$HERE/hog.py" "$LOAD" "$LIST" &
    HOG=$!
    sleep 2
fi

if [ "$TRACE" = 1 ]; then
    gfile 120 'C:\win3d\tearing\Get-FlipTrace.ps1' -Seconds 20 -Out 'C:\win3d\results\fliptrace.csv' >"$OUT/fliptrace.txt" 2>&1 &
    TRACER=$!
fi

rc=0
uv run --no-project --with numpy python "$HERE/rfbgrab.py" "$SOCK" --seconds "$SECONDS_RUN" --frames "$FRAMES" \
    --out "$OUT/torn" >"$OUT/grab.json" || rc=$?
[ -z "$HOG" ] || { kill "$HOG" 2>/dev/null || true; HOG=; }
cat "$OUT/grab.json"
if [ "$TRACE" = 1 ]; then
    wait "$TRACER" || true; TRACER=
    in_lab "$VMLAB" cp "$VM:C:/win3d/results/fliptrace.csv" "$OUT/fliptrace.csv" || true
fi
# The window closes by itself; its report says whether WPF rendered on the GPU.
wait "$WINDOW" || true; WINDOW=
in_lab "$VMLAB" exec --timeout 60 "$VM" -- cmd.exe /c type 'C:\win3d\results\tearwindow.json' 2>/dev/null | tr -d '\r' >"$OUT/window.json" || true
exit "$rc"
