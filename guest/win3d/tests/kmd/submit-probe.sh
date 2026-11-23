#!/usr/bin/env bash
# Runs probes/submit/submit_probe.exe in a running Windows lab as a standard
# user, one case per run, and keeps what the guest answered (card #752,
# finding F4 of the win3d driver review; docs/acceptance/C752.md).
#
#   tests/kmd/submit-probe.sh <lab-dir> <vm> <evidence-dir> <case>...
#
# Build the probe first: build/probes/build.sh submit. A driver without the
# fix can bugcheck the guest on the case "wrap": the script then waits for
# the guest to come back and records the bugcheck. Use a disposable lab.
# Exit status: 0 when every case passed, 1 otherwise, 2 on usage.
# check-shell-paths: no-errexit a case that fails or bugchecks is recorded and the next one runs (VML-88).
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
[ $# -ge 4 ] || { sed -n '2,12p' "$0" >&2; exit 2; }
LABDIR=$(cd "$1" && pwd); VM=$2; EV=$3; shift 3
PROBE=$REPO/out/probes-extra/x64/submit_probe.exe
[ -f "$PROBE" ] || { echo "no $PROBE: run build/probes/build.sh submit" >&2; exit 2; }
mkdir -p "$EV"
USER_NAME=c752probe
# A password for this run only, of a user in a disposable guest.
USER_PASS="P$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')a!"

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

wait_agent || exit 1
note "guest: driver, memory and the standard user"
gps "\$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force C:\\c752\\out | Out-Null
Get-LocalUser -Name $USER_NAME -ErrorAction SilentlyContinue | Remove-LocalUser
net.exe user $USER_NAME '$USER_PASS' /add | Out-Null
icacls.exe C:\\c752 /grant 'BUILTIN\\Users:(OI)(CI)RX' | Out-Null
icacls.exe C:\\c752\\out /grant 'BUILTIN\\Users:(OI)(CI)M' | Out-Null
Get-CimInstance Win32_PnPSignedDriver | Where-Object DeviceID -like 'PCI\\VEN_1AF4&DEV_1050*' | ForEach-Object { 'driver: {0} {1} {2}' -f \$_.DeviceName, \$_.DriverVersion, \$_.InfName }
'viogpu3d.sys sha256: ' + (Get-FileHash (Get-ChildItem C:\\Windows\\System32\\DriverStore\\FileRepository\\viogpu3d*\\viogpu3d.sys | Sort-Object LastWriteTime | Select-Object -Last 1).FullName -Algorithm SHA256).Hash.ToLower()
'memory: {0} MiB' -f [int]((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1MB)
'os: ' + (Get-CimInstance Win32_OperatingSystem).Version
'administrators: ' + ((Get-LocalGroupMember -Group Administrators).Name -join ', ')" 180 | tee "$EV/guest.txt" || { note "guest setup failed"; exit 1; }
in_lab vmlab cp "$PROBE" "$VM:C:\\c752\\submit_probe.exe" || exit 1
sha256sum "$PROBE" | sed "s|$REPO/||" >>"$EV/guest.txt"
in_lab vmlab exec --user "$USER_NAME" --password "$USER_PASS" --timeout 60 "$VM" -- cmd.exe /c "whoami /user /groups /fo csv" 2>&1 | tr -d '\r' >"$EV/whoami.txt"
grep -q 'S-1-5-32-544' "$EV/whoami.txt" && { note "the probe user is an administrator"; exit 1; }
bugchecks >"$EV/bugchecks-before.txt" 2>/dev/null

failed=0
for c in "$@"; do
    note "case $c"
    in_lab vmlab exec --user "$USER_NAME" --password "$USER_PASS" --timeout 120 "$VM" -- \
        'C:\c752\submit_probe.exe' --case "$c" --out "C:\\c752\\out\\$c.json" >"$EV/$c.stdout.json" 2>"$EV/$c.stderr.txt"
    rc=$?
    echo "$rc" >"$EV/$c.rc"
    if ! in_lab vmlab exec --user SYSTEM --timeout 20 "$VM" -- cmd.exe /c ver >/dev/null 2>&1; then
        note "case $c: the guest does not answer after the escape; waiting for it"
        in_lab vmlab status -v >"$EV/$c.status-v.txt" 2>&1
        in_lab vmlab vm screenshot "$VM" "$EV/$c.screen.png" >/dev/null 2>&1
        wait_agent || exit 1
    fi
    # What the guest has on disk: the record written before the escape when it did not return.
    in_lab vmlab cp "$VM:C:\\c752\\out\\$c.json" "$EV/$c.json" >/dev/null 2>&1
    bugchecks >"$EV/bugchecks-after-$c.txt" 2>/dev/null
    if ! cmp -s "$EV/bugchecks-before.txt" "$EV/bugchecks-after-$c.txt"; then
        note "case $c: BUGCHECK: $(head -1 "$EV/bugchecks-after-$c.txt")"
        cp "$EV/bugchecks-after-$c.txt" "$EV/bugchecks-before.txt"
        rc=bugcheck
    fi
    note "case $c: exit $rc: $(cat "$EV/$c.json" 2>/dev/null | tr -d '\r\n' | cut -c1-600)"
    [ "$rc" = 0 ] || failed=1
done
gps "Get-LocalUser -Name $USER_NAME -ErrorAction SilentlyContinue | Remove-LocalUser" 60
[ "$failed" = 0 ] && note "PASS: every case passed" || note "FAIL: a case failed"
exit "$failed"
