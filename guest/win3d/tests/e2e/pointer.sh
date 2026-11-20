#!/usr/bin/env bash
# The hardware pointer of viogpu3d (HWCursor, patches/kmd 0005), checked in a
# lab on vmlab-hv where the package is installed and the autologon desktop is
# up (a lab kept with tests/e2e/run.sh --keep).
#
#   tests/e2e/pointer.sh <lab-dir> <vm> <evidence-dir>
#
# tests/guest/Show-TestPointer.ps1 runs in the user's session: a flat
# full-screen window that steps the pointer through arrow, I-beam, resize,
# hidden and arrow, each at its own position. Meanwhile pointer.py records the
# pointer vmlab-hv publishes on its native frame protocol and on RFB, and
# `vmlab vm screenshot` captures the console. pointer.py then checks that
# every shape arrived at its position, that RFB carried them, that the I-beam
# is black with a white outline, and that no capture has the pointer in the
# frame. A `vmlab record` video of the run must show the pointer at each
# step's position, and none at the hidden step's. Writes <evidence-dir>/pointer/: pointer.json
# (the verdict), events.json, guest.json, pointer.mp4 and one capture. Exit
# status: 0 when all pass.
#
# WIN3D_POINTER_DIR names the VM's runtime directory (display.sock and
# vnc.sock) when the script cannot find it.
# check-shell-paths: no-errexit the guest run and the watcher record their rc and the check judges (VML-88).
set -uo pipefail
[ $# -eq 3 ] || { sed -n '2,23p' "$0" >&2; exit 2; }
REPO=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$REPO/tests/e2e
LABDIR=$(cd "$1" && pwd)
VM=$2
EV=$3/pointer
STAGE=$REPO/.scratch/e2e/pointer/$(basename "$3")
PLAN=arrow,ibeam,sizewe,hidden,arrow
SECONDS_EACH=6
X0=200 Y0=300 STEP_X=120
BACKGROUND=32,96,160

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { in_lab vmlab exec --timeout "${2:-600}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'; }

[ -n "$EV" ] && [ "$EV" != /pointer ] || { echo "VML-88: EV is empty or /" >&2; exit 2; }
[ -n "$STAGE" ] && [ "$STAGE" != / ] || { echo "VML-88: STAGE is empty or /" >&2; exit 2; }
mkdir -p "$EV" "$STAGE/shots"
find "$EV" -mindepth 1 -delete
find "$STAGE" -mindepth 1 -type f -delete

# The VM's sockets are beside the machine description its vmlab-hv was started
# with. vmlab shortens that path by hashing, so ask the processes: the one
# whose description names this lab directory and whose directory is the VM's.
DIR=${WIN3D_POINTER_DIR:-}
if [ -z "$DIR" ]; then
    while read -r m; do
        [ "$(basename "$(dirname "$m")")" = "$VM" ] && grep -qF "$LABDIR/" "$m" 2>/dev/null && DIR=$(dirname "$m")
    done < <(pgrep -u "$(id -u)" -af 'vmlab-hv .*--machine ' | sed -n 's/.*--machine \([^ ]*\).*/\1/p')
    [ -n "$DIR" ] || { echo "no running vmlab-hv for $VM of $LABDIR; set WIN3D_POINTER_DIR" >&2; exit 2; }
fi
[ -S "$DIR/display.sock" ] && [ -S "$DIR/vnc.sock" ] ||
    { echo "$DIR has no display.sock and vnc.sock: the pointer check needs vmlab-hv" >&2; exit 2; }

note "pointer check in $DIR"
in_lab vmlab cp "$REPO/tests/guest/Show-TestPointer.ps1" "$VM:C:/win3d/guest/Show-TestPointer.ps1" || exit 1
in_lab vmlab cp "$REPO/tests/guest/Run-Probe.ps1" "$VM:C:/win3d/guest/Run-Probe.ps1" || exit 1
gps 'Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceName -like "*VirtIO GPU*" } | ForEach-Object { "driver: {0} {1}" -f $_.DeviceName, $_.DriverVersion }
     Get-ChildItem "HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}" -ErrorAction SilentlyContinue | ForEach-Object { $v = (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).HWCursor; if ($null -ne $v) { "HWCursor: {0}" -f $v } }' 120 | tee "$EV/driver.txt"
# A recording of the run, at the size of the screen so that a frame's pixels are the screen's.
in_lab vmlab vm screenshot "$VM" "$STAGE/size.png" >/dev/null 2>&1
size=$(magick identify -format '%wx%h' "$STAGE/size.png" 2>/dev/null)
rm -f "$STAGE/size.png"
in_lab vmlab record start "$VM" -o "$STAGE/pointer.mp4" --size "${size:-1024x768}" || note "no recording"
# A real pointer event first: Windows shows no pointer before one.
in_lab vmlab vm mouse-move "$VM" 100 100 || true
in_lab vmlab vm sendkeys "$VM" esc || true

steps=$(tr ',' '\n' <<<"$PLAN" | grep -c .)
total=$((steps * SECONDS_EACH + 120))
python3 "$HERE/pointer.py" watch --display "$DIR/display.sock" --rfb "$DIR/vnc.sock" \
    --seconds "$total" --until "$STAGE/done" --out "$EV/events.json" &
watch_pid=$!
sleep 1
gps "& C:\\win3d\\guest\\Run-Probe.ps1 -Path C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -Raw -NoModules -Name pointer -OutDir C:\\win3d\\results\\pointer -Seconds $total -TimeoutSec $total -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-WindowStyle','Hidden','-File','C:\\win3d\\guest\\Show-TestPointer.ps1','-Shapes','$PLAN','-SecondsEach','$SECONDS_EACH','-X0','$X0','-Y0','$Y0','-StepX','$STEP_X','-Background','$BACKGROUND'; exit \$LASTEXITCODE" \
    $((total + 120)) >"$STAGE/run.out" 2>&1 &
guest_pid=$!
n=0
while kill -0 "$guest_pid" 2>/dev/null; do
    n=$((n + 1))
    in_lab vmlab vm screenshot "$VM" "$STAGE/shots/$(printf %03d $n).png" >/dev/null 2>&1 || true
    sleep 1
done
wait "$guest_pid"; note "guest run exit $?"
tail -3 "$STAGE/run.out"
sleep 2
touch "$STAGE/done"
wait "$watch_pid"; note "watch exit $?"
in_lab vmlab cp "$VM:C:/win3d/results/pointer/guest.json" "$EV/guest.json" || true
frames=()
if in_lab vmlab record stop "$VM" && [ -s "$STAGE/pointer.mp4" ]; then
    mkdir -p "$STAGE/frames"
    ffmpeg -loglevel error -y -i "$STAGE/pointer.mp4" -vf fps=5 "$STAGE/frames/%04d.png" && frames=(--frames "$STAGE/frames")
    cp "$STAGE/pointer.mp4" "$EV/pointer.mp4"
fi

python3 "$HERE/pointer.py" check --events "$EV/events.json" --plan "$PLAN" --x0 "$X0" --y0 "$Y0" --step-x "$STEP_X" \
    --background "$BACKGROUND" --shots "$STAGE/shots" --guest "$EV/guest.json" "${frames[@]}" --out "$EV/pointer.json"
rc=$?
# One capture of the window for the evidence: the middle one.
mid=$(find "$STAGE/shots" -name '*.png' | sort | sed -n "$(((n + 1) / 2))p")
[ -n "$mid" ] && magick "$mid" -resize 50% -strip -colors 256 "$EV/console-pointer.png"
find "$STAGE/shots" "$STAGE/frames" -name '*.png' -delete 2>/dev/null
exit $rc
