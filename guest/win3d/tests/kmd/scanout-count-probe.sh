#!/usr/bin/env bash
# The scanout count of the host as the driver takes it (review F5, card #754).
#
#   tests/kmd/scanout-count-probe.sh <lab-dir> <vm> <out-dir>
#
# The VM runs on a vmlab-hv built with tests/kmd/hv-forge-scanouts.patch and
# started with VMLAB_HV_FORGE_SCANOUTS=<n> (and VMLAB_HV_FORGE_EDID_FILL=<hex
# byte> to fill the forged EDID answers): its virtio-gpu reports n scanouts in
# the device configuration, enables n modes in the display info and answers
# GET_EDID for every scanout below n. The device still has one scanout.
#
# The probe writes into <out-dir>:
#   host.txt    the lines of the VM's vmlab-hv.log that the forged device wrote
#   guest.txt   the display device, its driver version and problem code, the
#               monitors Windows has, and the bugcheck events of the System log
#   verdict.txt one line for each check
# It passes (exit 0) when the host reported more than one scanout, the driver
# asked for the EDID of scanout 0 only, the display device has problem code 0,
# Windows has one monitor on it, and the System log has no bugcheck event.
# Exit 1 when a check fails, 2 on usage, 3 when the guest does not answer.
set -u
[ $# -eq 3 ] || { sed -n '2,19p' "$0" >&2; exit 2; }
LAB=$(cd "$1" && pwd) || exit 2
VM=$2
OUT=$3
VMLAB=${VMLAB:-vmlab}
[ -n "$OUT" ] && [ "$OUT" != / ] || exit 2
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
NAME=$(sed -n 's/^lab "\([^"]*\)".*/\1/p' "$LAB/vmlab.wcl" | head -1)
HVLOG=${XDG_STATE_HOME:-$HOME/.local/state}/vmlab/labs/$NAME/vms/$VM/vmlab-hv.log

grep 'forged host' "$HVLOG" > "$OUT/host.txt" 2>/dev/null
reported=$(sed -n 's/.*num_scanouts = \([0-9]*\).*/\1/p' "$OUT/host.txt" | tail -1)
asked_past=$(grep -c 'GET_EDID for scanout [1-9][0-9]* ' "$OUT/host.txt")
answered_past=$(grep -c 'GET_EDID for scanout [1-9][0-9]* answered' "$OUT/host.txt")

PS='$ErrorActionPreference = "Continue"
$d = @(Get-PnpDevice -Class Display -PresentOnly | Where-Object { $_.InstanceId -like "PCI\VEN_1AF4&DEV_1050*" })
foreach ($x in $d) {
  $v = (Get-PnpDeviceProperty -InputObject $x -KeyName DEVPKEY_Device_DriverVersion).Data
  "display: {0} status={1} problem={2} driver={3}" -f $x.FriendlyName, $x.Status, [int]$x.ConfigManagerErrorCode, $v
}
"display devices: {0}" -f $d.Count
$m = @(Get-PnpDevice -Class Monitor -PresentOnly)
foreach ($x in $m) { "monitor: {0} status={1}" -f $x.FriendlyName, $x.Status }
"monitors: {0}" -f $m.Count
$e = @(Get-WinEvent -FilterHashtable @{LogName="System"; ProviderName="Microsoft-Windows-WER-SystemErrorReporting"; Id=1001} -ErrorAction SilentlyContinue)
foreach ($x in $e) { "bugcheck: {0} {1}" -f $x.TimeCreated.ToUniversalTime().ToString("o"), ($x.Message -replace "\s+", " ") }
"bugcheck events: {0}" -f $e.Count
$k = @(Get-WinEvent -FilterHashtable @{LogName="System"; ProviderName="Microsoft-Windows-Kernel-Power"; Id=41} -ErrorAction SilentlyContinue)
"unclean shutdown events: {0}" -f $k.Count'
answered=0
for _ in 1 2 3 4 5 6; do
    if (cd "$LAB" && "$VMLAB" exec --user SYSTEM --timeout 120 "$VM" -- powershell.exe -NoProfile -NonInteractive -Command "$PS") 2>"$OUT/guest.err" | tr -d '\r' > "$OUT/guest.txt" &&
        grep -q '^bugcheck events: ' "$OUT/guest.txt"; then
        answered=1
        break
    fi
    sleep 20
done

check() { # check NAME STATUS DETAIL: one verdict line, PASS when STATUS is 0
    if [ "$2" = 0 ]; then echo "PASS $1: $3"; else echo "FAIL $1: $3"; fi
}
{
    [ "${reported:-0}" -gt 1 ]; check host-forged $? "the host reported num_scanouts = ${reported:-none}"
    [ "$asked_past" = 0 ]; check edid-requests $? "GET_EDID requests past scanout 0: $asked_past ($answered_past answered)"
    if [ $answered = 1 ]; then
        problem=$(sed -n 's/^display: .* problem=\([0-9]*\) .*/\1/p' "$OUT/guest.txt" | head -1)
        [ "${problem:-x}" = 0 ]; check device-started $? "$(grep '^display: ' "$OUT/guest.txt" | head -1)"
        [ "$(sed -n 's/^monitors: //p' "$OUT/guest.txt")" = 1 ]; check one-monitor $? "$(grep '^monitors: ' "$OUT/guest.txt")"
        [ "$(sed -n 's/^bugcheck events: //p' "$OUT/guest.txt")" = 0 ]; check no-bugcheck $? "$(grep '^bugcheck events: ' "$OUT/guest.txt")"
    else
        echo "FAIL guest: the guest agent did not answer"
    fi
} | tee "$OUT/verdict.txt"
grep -q '^FAIL guest' "$OUT/verdict.txt" && exit 3
grep -q '^FAIL' "$OUT/verdict.txt" && exit 1
exit 0
