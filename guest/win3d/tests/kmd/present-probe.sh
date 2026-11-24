#!/usr/bin/env bash
# Runs the present probe (probes/present) in a Windows VM as a standard user
# in the desktop session: a Blt present of a blob that cannot be mapped,
# card #756 (docs/reviews/win3d-driver.md F8; docs/acceptance/C756.md).
#
#   tests/kmd/present-probe.sh <lab-dir> <vm> <out-dir> [case...]
#
# <lab-dir> holds the lab's vmlab.wcl and <vm> is the VM name there. The VM
# runs and has the package installed. The script copies tests/guest and
# out/probes-extra/x64/present_probe.exe (build/probes/build.sh present)
# into the guest. The probe needs a window with a surface, so it runs in the
# session of the autologon user win3d, which tests/guest/Set-TestDesktop.ps1
# makes as a member of Users only. The script makes that desktop when the
# guest has none (one reboot), and refuses an administrator on the console.
# PRESENT_COUNT is the number of presents per case (default 1).
#
# A driver without the fix accepts the present and bugchecks the guest.
# The script then waits for the guest to come back and records the bugcheck
# event. Use a disposable VM: the script also turns off the recovery screen
# that Windows shows after two bugchecks.
#
# <out-dir> gets driver.txt (the installed viogpu3d.sys), user.txt, per
# case <case>.json (the probe's result; "present_pending" or
# "present_accepted" when the guest died), <case>.bugcheck.txt, and
# summary.txt.
#
# Exit status: 0 when every case passed and the guest never bugchecked, 1
# otherwise, 2 on usage.
# check-shell-paths: no-errexit a case that kills the guest is a result to record, and the next case still runs (VML-88).
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
if [ $# -lt 3 ]; then
    sed -n '2,/^# check-shell-paths:/{ /^# check-shell-paths:/!p; }' "$0" >&2
    exit 2
fi
LABDIR=$(cd "$1" && pwd)
VM=$2
mkdir -p "$3"
OUT=$(cd "$3" && pwd)
[ -n "$OUT" ] && [ "$OUT" != / ] || { echo "VML-88: the output directory is empty or /" >&2; exit 2; }
shift 3
CASES=${*:-not-mappable unknown-blob}
COUNT=${PRESENT_COUNT:-1}
PROBE=$REPO/out/probes-extra/x64/present_probe.exe
[ -f "$PROBE" ] || { echo "no $PROBE; run build/probes/build.sh present" >&2; exit 2; }
VMLAB=${VMLAB:-vmlab}
ACCOUNT=win3d
GUEST='C:\win3d\present'

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { # gps 'powershell' [timeout]: run PowerShell as SYSTEM in the guest
    in_lab "$VMLAB" exec --user SYSTEM --timeout "${2:-120}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'
}
gfile() { # gfile timeout script.ps1 args...: run a guest script file as SYSTEM
    local t=$1; shift
    in_lab "$VMLAB" exec --user SYSTEM --timeout "$t" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$@" | tr -d '\r'
}
wait_agent() {
    for _ in $(seq 1 90); do
        in_lab "$VMLAB" exec --user SYSTEM --timeout 20 "$VM" -- cmd.exe /c ver >/dev/null 2>&1 && return 0
        sleep 10
    done
    note "guest agent did not answer"; return 1
}
console_user() { gps '(Get-CimInstance Win32_ComputerSystem).UserName' 60 2>/dev/null | tail -1; }
wait_desktop() { # the shell of the autologon user, in a session that is not 0
    for _ in $(seq 1 60); do
        case $(gps '[bool](Get-Process explorer -ErrorAction SilentlyContinue | Where-Object SessionId -gt 0)' 60 2>/dev/null | tail -1) in
            True) return 0 ;;
        esac
        sleep 10
    done
    note "no desktop session"; return 1
}
last_bugcheck() { # the record id of the newest bugcheck event, 0 without one
    gps '$e = Get-WinEvent -FilterHashtable @{LogName="System"; ProviderName="Microsoft-Windows-WER-SystemErrorReporting"; Id=1001} -MaxEvents 1 -ErrorAction SilentlyContinue; if ($e) { $e.RecordId } else { 0 }' 60 | tail -1
}
field() { # field FILE NAME: a value of the probe's JSON
    python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1], encoding="utf-8-sig")).get(sys.argv[2]))
except Exception: print("unreadable")' "$1" "$2"
}

wait_agent || exit 1
gps "New-Item -ItemType Directory -Force '$GUEST\\out' | Out-Null; icacls.exe '$GUEST\\out' /grant '*S-1-5-32-545:(OI)(CI)M' | Out-Null" 60 || exit 1
in_lab "$VMLAB" cp "$REPO/tests/guest" "$VM:C:/win3d/guest" || exit 1
in_lab "$VMLAB" cp "$PROBE" "$VM:C:/win3d/present/present_probe.exe" || exit 1

case $(console_user) in
    *"\\$ACCOUNT") ;;
    *)
        note "no desktop of $ACCOUNT: Set-TestDesktop.ps1 -Reboot"
        gfile 1200 'C:\win3d\guest\Set-TestDesktop.ps1' -Reboot >"$OUT/test-desktop.txt" 2>&1 || true
        sleep 60; wait_agent || exit 1
        ;;
esac
wait_desktop || exit 1
gps "\$a = (Get-LocalGroupMember -SID S-1-5-32-544 | ForEach-Object Name) -join ', '
    \$u = (Get-LocalGroupMember -SID S-1-5-32-545 | ForEach-Object Name) -join ', '
    'console user: ' + (Get-CimInstance Win32_ComputerSystem).UserName
    'administrators: ' + \$a
    'users: ' + \$u
    'standard user: ' + [bool]((\$u -split ', ') -like '*\\$ACCOUNT' -and -not ((\$a -split ', ') -like '*\\$ACCOUNT'))" 120 >"$OUT/user.txt"
note "$(tr '\n' ' ' <"$OUT/user.txt")"
grep -q "^console user: .*\\\\$ACCOUNT\$" "$OUT/user.txt" && grep -q '^standard user: True$' "$OUT/user.txt" ||
    { note "the console user is not the standard user $ACCOUNT; see user.txt"; exit 1; }

gps '$f = Get-Item C:\Windows\System32\drivers\viogpu3d.sys -ErrorAction SilentlyContinue
    if (-not $f) { $f = Get-ChildItem C:\Windows\System32\DriverStore\FileRepository -Recurse -Filter viogpu3d.sys | Sort-Object LastWriteTime | Select-Object -Last 1 }
    $d = Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.HardwareID -like "PCI\VEN_1AF4&DEV_1050*" } | Select-Object -First 1
    "driver version: {0}" -f $d.DriverVersion
    "file: {0}" -f $f.FullName
    "sha256: {0}" -f (Get-FileHash $f.FullName -Algorithm SHA256).Hash.ToLower()' 120 >"$OUT/driver.txt"
note "$(tr '\n' ' ' <"$OUT/driver.txt")"

gps 'bcdedit /set "{current}" bootstatuspolicy IgnoreAllFailures | Out-Null; bcdedit /set "{current}" recoveryenabled No | Out-Null' 60

bad=0
: >"$OUT/summary.txt"
for c in $CASES; do
    before=$(last_bugcheck); before=${before:-0}
    gps "Remove-Item '$GUEST\\out\\$c.json' -ErrorAction SilentlyContinue" 60
    rc=0
    gps "& 'C:\\win3d\\guest\\Run-Probe.ps1' -Path '$GUEST\\present_probe.exe' -Raw -NoModules -Name 'present-$c' -OutDir '$GUEST\\run' -TimeoutSec 120 -ArgumentList '--case','$c','--count','$COUNT','--out','$GUEST\\out\\$c.json'; exit \$LASTEXITCODE" 420 >"$OUT/$c.run.txt" 2>&1 || rc=$?
    sleep 5
    if ! in_lab "$VMLAB" exec --user SYSTEM --timeout 20 "$VM" -- cmd.exe /c ver >/dev/null 2>&1; then
        note "$c: the guest went away; waiting for it to come back"
        in_lab "$VMLAB" vm screenshot "$VM" "$OUT/$c.screen.png" >/dev/null 2>&1
        sleep 30; wait_agent || { echo "$c: exit=$rc guest did not come back" >>"$OUT/summary.txt"; exit 1; }
    fi
    gps "Get-Content '$GUEST\\out\\$c.json' -Raw -ErrorAction SilentlyContinue" 60 >"$OUT/$c.json"
    # The probe leaves "present_pending" or "present_accepted" on disk when the guest died under it.
    died=no
    case $(field "$OUT/$c.json" result) in present_pending | present_accepted) [ "$(field "$OUT/$c.json" stage)" = "done" ] || died=yes ;; esac
    after=$(last_bugcheck); after=${after:-0}
    if [ "$died" = yes ]; then
        # Windows writes the bugcheck event some time after the boot.
        for _ in $(seq 1 24); do
            [ "$after" != "$before" ] && break
            sleep 10; after=$(last_bugcheck); after=${after:-0}
        done
        wait_desktop
    fi
    bug=none
    if [ "$after" != "$before" ]; then
        gps '$e = Get-WinEvent -FilterHashtable @{LogName="System"; ProviderName="Microsoft-Windows-WER-SystemErrorReporting"; Id=1001} -MaxEvents 1; "record {0} at {1}" -f $e.RecordId, $e.TimeCreated.ToUniversalTime().ToString("o"); $e.Message' 60 >"$OUT/$c.bugcheck.txt"
        bug=$(grep -oE '0x[0-9a-fA-F]{8} \(0x[0-9a-fA-F]+, 0x[0-9a-fA-F]+, 0x[0-9a-fA-F]+, 0x[0-9a-fA-F]+\)' "$OUT/$c.bugcheck.txt" | head -1)
        bug=${bug:-recorded}
    fi
    line="$c: exit=$rc result=$(field "$OUT/$c.json" result) stage=$(field "$OUT/$c.json" stage) session=$(field "$OUT/$c.json" session) presents=$(field "$OUT/$c.json" presents_done) failed=$(field "$OUT/$c.json" presents_failed) accepted=$(field "$OUT/$c.json" presents_accepted) first_present_status=$(field "$OUT/$c.json" first_present_status) pass=$(field "$OUT/$c.json" pass) guest_died=$died bugcheck=$bug"
    note "$line"; echo "$line" >>"$OUT/summary.txt"
    if [ "$rc" != 0 ] || [ "$died" = yes ] || [ "$bug" != none ] || [ "$(field "$OUT/$c.json" pass)" != True ]; then bad=1; fi
done
if [ "$bad" = 0 ]; then echo "PASS: every present was refused and the guest stayed up" | tee -a "$OUT/summary.txt"; else echo "FAIL" | tee -a "$OUT/summary.txt"; fi
exit "$bad"
