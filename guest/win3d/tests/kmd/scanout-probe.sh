#!/usr/bin/env bash
# Runs probes/scanout/scanout_probe.exe in a running Windows lab as a standard
# user, and takes a screenshot from the host while the probe asks (card #755,
# finding F7 of the win3d driver review; docs/acceptance/C755.md).
#
#   tests/kmd/scanout-probe.sh <lab-dir> <vm> <evidence-dir> <case>...
#
# The cases are render-flip and escape. Build the probe first:
# build/probes/build.sh scanout. The lab of the acceptance tests logs nobody
# on, so its screen is the sign-in screen of Windows, which is on the secure
# desktop: guest.txt has the answer of quser. A case passes when the driver refused the
# probe and the screenshot taken meanwhile shows what Windows shows: none of
# the probe's colour, and not one colour alone. A driver without the fix
# shows the colour in place of the desktop, or the host's "Display output is
# not active" where the host cannot show the resource, and the screen stays
# so until Windows draws again: use a disposable lab.
# Exit status: 0 when every case passed, 1 otherwise, 2 on usage.
# check-shell-paths: no-errexit a case that fails is recorded and the next one runs (VML-88).
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
[ $# -ge 4 ] || { sed -n '2,17p' "$0" >&2; exit 2; }
LABDIR=$(cd "$1" && pwd); VM=$2; EV=$3; shift 3
PROBE=$REPO/out/probes-extra/x64/scanout_probe.exe
[ -f "$PROBE" ] || { echo "no $PROBE: run build/probes/build.sh scanout" >&2; exit 2; }
mkdir -p "$EV"
[ -n "$EV" ] && [ -d "$EV" ] || exit 1
USER_NAME=c755probe
# A password for this run only, of a user in a disposable guest.
USER_PASS="P$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')a!"
SECONDS_OF_PROBE=${SECONDS_OF_PROBE:-14}

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { in_lab vmlab exec --user SYSTEM --timeout "${2:-120}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'; }
wait_agent() {
    for _ in $(seq 1 90); do
        in_lab vmlab exec --user SYSTEM --timeout 20 "$VM" -- cmd.exe /c ver >/dev/null 2>&1 && return 0
        sleep 10
    done
    note "guest agent did not answer"; return 1
}
bugchecks() { # the bugcheck records of the System log, newest first
    gps 'Get-WinEvent -FilterHashtable @{LogName="System"; Id=1001; ProviderName="Microsoft-Windows-WER-SystemErrorReporting"} -ErrorAction SilentlyContinue | ForEach-Object { "{0:o} {1}" -f $_.TimeCreated, ($_.Message -replace "\s+", " ") }' 120
}
# Of a screenshot: its size, the share of its pixels that have the probe's
# colour, and the share of its most frequent colour, in percent.
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
counts = {}
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
        key = bytes(line[x:x + 3])
        counts[key] = counts.get(key, 0) + 1
    prev = line
print('%dx%d %.1f %.1f' % (width, height, 100.0 * hits / (width * height), 100.0 * max(counts.values()) / (width * height)))
PY
}

# Windows sends no frame while nothing changes. After a case that took the
# screen away, a key makes it draw again.
live_screen() {
    for _ in 1 2 3 4 5 6; do
        in_lab vmlab vm screenshot "$VM" "$EV/$1.before.png" >/dev/null 2>&1
        set -- "$1" $(colour_share "$EV/$1.before.png" 2>/dev/null | tail -1)
        [ -n "${4:-}" ] && awk -v d="$4" 'BEGIN { exit !(d < 90) }' && return 0
        in_lab vmlab vm sendkeys "$VM" super >/dev/null 2>&1
        sleep 4
    done
    return 1
}

wait_agent || exit 1
note "guest: driver and the standard user"
gps "\$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force C:\\c755\\out | Out-Null
Get-LocalUser -Name $USER_NAME -ErrorAction SilentlyContinue | Remove-LocalUser
New-LocalUser -Name $USER_NAME -Password (ConvertTo-SecureString '$USER_PASS' -AsPlainText -Force) -PasswordNeverExpires | Out-Null
Add-LocalGroupMember -SID S-1-5-32-545 -Member $USER_NAME
icacls.exe C:\\c755 /grant 'BUILTIN\\Users:(OI)(CI)RX' | Out-Null
icacls.exe C:\\c755\\out /grant 'BUILTIN\\Users:(OI)(CI)M' | Out-Null
Get-CimInstance Win32_PnPSignedDriver | Where-Object DeviceID -like 'PCI\\VEN_1AF4&DEV_1050*' | ForEach-Object { 'driver: {0} {1} {2}' -f \$_.DeviceName, \$_.DriverVersion, \$_.InfName }
'viogpu3d.sys sha256: ' + (Get-FileHash (Get-ChildItem C:\\Windows\\System32\\DriverStore\\FileRepository\\viogpu3d*\\viogpu3d.sys | Sort-Object LastWriteTime | Select-Object -Last 1).FullName -Algorithm SHA256).Hash.ToLower()
'os: ' + (Get-CimInstance Win32_OperatingSystem).Version
'administrators: ' + ((Get-LocalGroupMember -Group Administrators).Name -join ', ')" 180 | tee "$EV/guest.txt" || { note "guest setup failed"; exit 1; }
in_lab vmlab cp "$PROBE" "$VM:C:\\c755\\scanout_probe.exe" || exit 1
sha256sum "$PROBE" | sed "s|$REPO/||" >>"$EV/guest.txt"
in_lab vmlab exec --user "$USER_NAME" --password "$USER_PASS" --timeout 60 "$VM" -- cmd.exe /c "whoami /user /groups /fo csv" 2>&1 | tr -d '\r' >"$EV/whoami.txt"
grep -q 'S-1-5-32-545' "$EV/whoami.txt" || { note "no logon as the probe user: $(head -1 "$EV/whoami.txt")"; exit 1; }
grep -q 'S-1-5-32-544' "$EV/whoami.txt" && { note "the probe user is an administrator"; exit 1; }
gps 'quser.exe 2>&1 | Out-String' 60 >>"$EV/guest.txt"
bugchecks >"$EV/bugchecks-before.txt" 2>/dev/null

failed=0
for c in "$@"; do
    note "case $c"
    rm -f "$EV/$c.json" "$EV/$c.stdout.json" "$EV/$c.stderr.txt" "$EV/$c.rc" "$EV/$c".*.png "$EV/$c.share.txt"
    live_screen "$c" || { note "case $c: the screen shows one colour before the probe ran"; failed=1; continue; }
    (
        in_lab vmlab exec --user "$USER_NAME" --password "$USER_PASS" --timeout 120 "$VM" -- \
            'C:\c755\scanout_probe.exe' --case "$c" --seconds "$SECONDS_OF_PROBE" --out "C:\\c755\\out\\$c.json" >"$EV/$c.stdout.json" 2>"$EV/$c.stderr.txt"
        echo $? >"$EV/$c.rc"
    ) &
    sleep $((SECONDS_OF_PROBE / 2))
    in_lab vmlab vm screenshot "$VM" "$EV/$c.during.png" >/dev/null 2>&1
    wait
    rc=$(cat "$EV/$c.rc" 2>/dev/null || echo none)
    sleep 5
    in_lab vmlab vm screenshot "$VM" "$EV/$c.after.png" >/dev/null 2>&1
    wait_agent || exit 1
    in_lab vmlab cp "$VM:C:\\c755\\out\\$c.json" "$EV/$c.json" >/dev/null 2>&1
    for shot in before during after; do
        printf '%s %s\n' "$shot" "$(colour_share "$EV/$c.$shot.png" 2>&1 | tail -1)"
    done >"$EV/$c.share.txt"
    share=$(awk '$1 == "during" { print $3 }' "$EV/$c.share.txt")
    one=$(awk '$1 == "during" { print $4 }' "$EV/$c.share.txt")
    bugchecks >"$EV/bugchecks-after-$c.txt" 2>/dev/null
    if ! cmp -s "$EV/bugchecks-before.txt" "$EV/bugchecks-after-$c.txt"; then
        note "case $c: BUGCHECK: $(head -1 "$EV/bugchecks-after-$c.txt")"
        cp "$EV/bugchecks-after-$c.txt" "$EV/bugchecks-before.txt"
        rc=bugcheck
    fi
    note "case $c: exit $rc, meanwhile the probe's colour on ${share:-?} % of the screen and one colour on ${one:-?} %: $(tr -d '\r\n' <"$EV/$c.json" 2>/dev/null | cut -c1-700)"
    case "$share" in 0.0) ;; *) rc="$rc+colour" ;; esac
    awk -v d="${one:-100}" 'BEGIN { exit !(d < 90) }' || rc="$rc+no-desktop"
    [ "$rc" = 0 ] || failed=1
done
gps "Get-LocalUser -Name $USER_NAME -ErrorAction SilentlyContinue | Remove-LocalUser" 60
[ "$failed" = 0 ] && note "PASS: every case passed" || note "FAIL: a case failed"
exit "$failed"
