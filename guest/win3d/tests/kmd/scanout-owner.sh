#!/usr/bin/env bash
# The other side of the check of card #755: a fullscreen exclusive program,
# which Windows gives the scanout, still shows its frames. Runs
# probes/scanout/scanout_owner.exe on the desktop of a running Windows lab
# and takes screenshots from the host meanwhile (docs/acceptance/C755.md).
#
#   tests/kmd/scanout-owner.sh <lab-dir> <vm> <evidence-dir>
#
# Build the program first: build/probes/build.sh scanout. The script logs a
# test user on at the console (tests/guest/Set-TestDesktop.ps1, one reboot).
# It passes when every present of the program succeeded and one screenshot
# shows the program's colour on the whole screen.
# Exit status: 0 passed, 1 failed, 2 on usage.
# check-shell-paths: no-errexit the evidence of a failed run is collected before the exit (VML-88).
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
[ $# -eq 3 ] || { sed -n '2,13p' "$0" >&2; exit 2; }
LABDIR=$(cd "$1" && pwd); VM=$2; EV=$3
PROGRAM=$REPO/out/probes-extra/x64/scanout_owner.exe
[ -f "$PROGRAM" ] || { echo "no $PROGRAM: run build/probes/build.sh scanout" >&2; exit 2; }
mkdir -p "$EV"
[ -n "$EV" ] && [ -d "$EV" ] || exit 1
SECONDS_OF_PROGRAM=${SECONDS_OF_PROGRAM:-20}

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gfile() { # gfile timeout script.ps1 args...: run a guest script file as SYSTEM
    local t=$1; shift
    in_lab vmlab exec --user SYSTEM --timeout "$t" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$@" | tr -d '\r'
}
wait_agent() {
    for _ in $(seq 1 90); do
        in_lab vmlab exec --user SYSTEM --timeout 20 "$VM" -- cmd.exe /c ver >/dev/null 2>&1 && return 0
        sleep 10
    done
    note "guest agent did not answer"; return 1
}
# The share of the pixels of a screenshot that have the program's colour, in percent.
colour_share() {
    python3 - "$1" <<'PY'
import struct, sys, zlib
data = open(sys.argv[1], 'rb').read()
assert data[:8] == b'\x89PNG\r\n\x1a\n', 'not a PNG'
pos, idat = 8, b''
while pos < len(data):
    length, kind = struct.unpack('>I4s', data[pos:pos + 8])
    body = data[pos + 8:pos + 8 + length]
    if kind == b'IHDR':
        width, height, depth, colour, _, _, interlace = struct.unpack('>IIBBBBB', body)
    elif kind == b'IDAT':
        idat += body
    pos += 12 + length
assert depth == 8 and colour in (2, 6) and interlace == 0, 'an 8-bit RGB or RGBA PNG is expected'
bpp = 3 if colour == 2 else 4
raw = zlib.decompress(idat)
stride = width * bpp
prev = bytearray(stride)
hits = 0
for y in range(height):
    start = y * (stride + 1)
    kind = raw[start]
    line = bytearray(raw[start + 1:start + 1 + stride])
    for i in range(stride):
        a = line[i - bpp] if i >= bpp else 0
        b = prev[i]
        c = prev[i - bpp] if i >= bpp else 0
        if kind == 1: line[i] = (line[i] + a) & 255
        elif kind == 2: line[i] = (line[i] + b) & 255
        elif kind == 3: line[i] = (line[i] + (a + b) // 2) & 255
        elif kind == 4:
            p = a + b - c
            pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
    for x in range(0, stride, bpp):
        if line[x] >= 240 and line[x + 1] <= 15 and line[x + 2] >= 240:
            hits += 1
    prev = line
print('%dx%d %.1f' % (width, height, 100.0 * hits / (width * height)))
PY
}

wait_agent || exit 1
in_lab vmlab cp "$REPO/tests/guest" "$VM:C:/win3d/guest" || exit 1
in_lab vmlab cp "$PROGRAM" "$VM:C:/c755/owner/x64/scanout_owner.exe" || exit 1
sha256sum "$PROGRAM" | sed "s|$REPO/||" >"$EV/guest.txt"
note "a test user is logged on at the console"
gfile 1200 'C:\win3d\guest\Set-TestDesktop.ps1' -Reboot >"$EV/test-desktop.txt" 2>&1 || true
sleep 30
wait_agent || exit 1
sleep 60
in_lab vmlab exec --user SYSTEM --timeout 60 "$VM" -- quser.exe 2>&1 | tr -d '\r' >>"$EV/guest.txt"
in_lab vmlab exec --user SYSTEM --timeout 120 "$VM" -- powershell.exe -NoProfile -NonInteractive -Command \
    "Get-CimInstance Win32_PnPSignedDriver | Where-Object DeviceID -like 'PCI\\VEN_1AF4&DEV_1050*' | ForEach-Object { 'driver: {0} {1}' -f \$_.DeviceName, \$_.DriverVersion }" 2>&1 | tr -d '\r' >>"$EV/guest.txt"

# Without this layer Windows 11 composes the program like a window ("fullscreen
# optimizations"), and its frames do not come through the exclusive path.
in_lab vmlab exec --user SYSTEM --timeout 60 "$VM" -- reg.exe add 'HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers' \
    /v 'C:\c755\owner\x64\scanout_owner.exe' /t REG_SZ /d '~ DISABLEDXMAXIMIZEDWINDOWEDMODE' /f 2>&1 | tr -d '\r' >>"$EV/guest.txt"
rm -f "$EV"/owner.*.png "$EV/owner.json" "$EV/owner.share.txt" "$EV/owner.rc"
in_lab vmlab vm screenshot "$VM" "$EV/owner.before.png" >/dev/null 2>&1
(
    gfile 600 'C:\win3d\guest\Run-Probe.ps1' -Path 'C:\c755\owner\x64\scanout_owner.exe' -Seconds "$SECONDS_OF_PROGRAM" \
        -TimeoutSec "$((SECONDS_OF_PROGRAM + 60))" -OutDir 'C:\c755\owner\out' -Name owner -NoModules >"$EV/owner.run.json" 2>&1
    echo $? >"$EV/owner.rc"
) &
best=0.0
for n in 1 2 3 4 5 6 7 8; do
    sleep 3
    in_lab vmlab vm screenshot "$VM" "$EV/owner.$n.png" >/dev/null 2>&1
    share=$(colour_share "$EV/owner.$n.png" 2>&1 | tail -1)
    echo "$n $share" >>"$EV/owner.share.txt"
    best=$(echo "$share" | awk -v b="$best" '{ print ($2 > b) ? $2 : b }')
done
wait
rc=$(cat "$EV/owner.rc" 2>/dev/null || echo none)
in_lab vmlab cp "$VM:C:/c755/owner/out/owner.json" "$EV/owner.json" >/dev/null 2>&1
note "the program: exit $rc, its colour on at most $best % of the screen: $(tr -d '\r\n' <"$EV/owner.json" 2>/dev/null | cut -c1-400)"
if [ "$rc" = 0 ] && grep -q '"pass":true' "$EV/owner.json" 2>/dev/null && awk -v b="$best" 'BEGIN { exit !(b >= 99) }'; then
    note "PASS: the fullscreen program showed its frames"; exit 0
fi
note "FAIL: the fullscreen program did not show its frames"
exit 1
