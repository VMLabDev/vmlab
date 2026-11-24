#!/usr/bin/env bash
# Runs the allocation probe (probes/alloc) in a Windows VM as a standard
# user: the requests of card #757 to D3DKMTCreateAllocation
# (docs/reviews/win3d-driver.md F10).
#
#   tests/kmd/alloc-probe.sh <lab-dir> <vm> <out-dir> [case...]
#
# <lab-dir> holds the lab's vmlab.wcl and <vm> is the VM name there. The VM
# runs and has the package installed. The script copies
# out/probes-extra/x64/alloc_probe.exe (build/probes/build.sh alloc) into the
# guest, makes the local account alloc-probe, a member of Users only, with a
# random password, and runs one case per process as that account through
# `vmlab exec --user`. With no case names it runs one, one-v1, two, two-v1
# and none.
#
# The case "starve" is the failed pool allocation. For it the script turns on
# Driver Verifier for viogpu3d.sys with the randomized low resources
# simulation, set to fail every pool allocation the driver makes in the
# process alloc_probe.exe from STARVE_DELAY minutes (default 3) after the
# boot, and restarts the guest. The probe starts right after the boot, makes
# its device while allocations still succeed, and creates allocations until
# they fail. Afterwards the script resets Driver Verifier and restarts the
# guest again.
#
# A driver without the fix bugchecks the guest on "starve". The script then
# waits for the guest to come back and records the bugcheck event. Use a
# disposable VM: the script also turns off the recovery screen that Windows
# shows after two bugchecks (bcdedit bootstatuspolicy and recoveryenabled).
#
# <out-dir> gets driver.txt (the installed viogpu3d.sys), <case>.json (the
# probe's result; "request_did_not_return" or "starving" when the guest died
# in the call), <case>.bugcheck.txt, verifier.txt and summary.txt.
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
CASES=${*:-one one-v1 two two-v1 none}
PROBE=$REPO/out/probes-extra/x64/alloc_probe.exe
[ -f "$PROBE" ] || { echo "no $PROBE; run build/probes/build.sh alloc" >&2; exit 2; }
VMLAB=${VMLAB:-vmlab}
ACCOUNT=alloc-probe
GUEST='C:\win3d\alloc'
STARVE_DELAY=${STARVE_DELAY:-3}
[[ $STARVE_DELAY =~ ^[1-9][0-9]?$ ]] || { echo "STARVE_DELAY is a number of minutes" >&2; exit 2; }

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { # gps 'powershell' [timeout]: run PowerShell as SYSTEM in the guest
    in_lab "$VMLAB" exec --user SYSTEM --timeout "${2:-120}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'
}
agent_answers() { in_lab "$VMLAB" exec --user SYSTEM --timeout 20 "$VM" -- cmd.exe /c ver >/dev/null 2>&1; }
wait_agent() {
    for _ in $(seq 1 90); do
        agent_answers && return 0
        sleep 10
    done
    note "guest agent did not answer"; return 1
}
restart_guest() { # a restart from inside the guest, and the wait for its agent
    local boot
    boot=$(gps '(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString("o")' 60 | tail -1)
    gps 'shutdown.exe /r /t 0 /f' 60 >/dev/null
    for _ in $(seq 1 60); do
        sleep 5
        [ "$(gps '(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString("o")' 30 2>/dev/null | tail -1)" != "$boot" ] || continue
        agent_answers && return 0
    done
    note "the guest did not come back from the restart"; return 1
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
gps "New-Item -ItemType Directory -Force '$GUEST\\out' | Out-Null" 60 || exit 1
in_lab "$VMLAB" cp "$PROBE" "$VM:C:/win3d/alloc/alloc_probe.exe" || exit 1

# A standard user. The password is random and stays in this process.
PASSWORD="Ap-$(head -c 18 /dev/urandom | base64 | tr -d '/+=')-9z"
gps "\$p = ConvertTo-SecureString '$PASSWORD' -AsPlainText -Force
    if (Get-LocalUser -Name '$ACCOUNT' -ErrorAction SilentlyContinue) { Set-LocalUser -Name '$ACCOUNT' -Password \$p }
    else { New-LocalUser -Name '$ACCOUNT' -Password \$p -PasswordNeverExpires | Out-Null }
    if (-not (Get-LocalGroupMember -SID S-1-5-32-545 | Where-Object Name -like '*\\$ACCOUNT')) { Add-LocalGroupMember -SID S-1-5-32-545 -Member '$ACCOUNT' }
    icacls '$GUEST\\out' /grant '${ACCOUNT}:(OI)(CI)M' | Out-Null
    'account {0}: groups {1}' -f '$ACCOUNT', ((Get-LocalGroup | Where-Object { Get-LocalGroupMember \$_ -ErrorAction SilentlyContinue | Where-Object Name -like '*\\$ACCOUNT' }).Name -join ', ')" 120 >"$OUT/account.txt" || exit 1
note "$(cat "$OUT/account.txt")"
in_lab "$VMLAB" exec --user "$ACCOUNT" --password "$PASSWORD" --timeout 60 "$VM" -- cmd.exe /c "whoami /groups /fo csv /nh" 2>&1 | tr -d '\r' >"$OUT/whoami.txt"
if grep -q 'S-1-5-32-544' "$OUT/whoami.txt" || ! grep -q 'S-1-5-32-545' "$OUT/whoami.txt"; then
    note "the account is not a standard user; see whoami.txt"; exit 1
fi

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
    limit=90
    args=()
    if [ "$c" = starve ]; then
        # Every pool allocation of viogpu3d.sys in alloc_probe.exe fails
        # (10000 of 10000), with any pool tag, from STARVE_DELAY minutes
        # after the boot. A boot that fails turns the verifier off again.
        gps "verifier.exe /bootmode resetonbootfail | Out-Null
            verifier.exe /flags 0x4 /faults 10000 '' alloc_probe.exe $STARVE_DELAY /driver viogpu3d.sys" 60 >"$OUT/verifier.txt" 2>&1
        note "restarting the guest with Driver Verifier on viogpu3d.sys"
        restart_guest || exit 1
        gps 'verifier.exe /querysettings' 60 >>"$OUT/verifier.txt" 2>&1
        limit=$((STARVE_DELAY * 60 + 400))
        args=(--seconds $((STARVE_DELAY * 60 + 240)))
    fi
    before=$(last_bugcheck); before=${before:-0}
    gps "Remove-Item '$GUEST\\out\\$c.json' -ErrorAction SilentlyContinue" 60
    rc=0
    in_lab "$VMLAB" exec --user "$ACCOUNT" --password "$PASSWORD" --timeout "$limit" "$VM" -- \
        "$GUEST\\alloc_probe.exe" --case "$c" "${args[@]}" --out "$GUEST\\out\\$c.json" >"$OUT/$c.stdout.txt" 2>&1 || rc=$?
    sleep 5
    if ! agent_answers; then
        note "$c: the guest went away; waiting for it to come back"
        sleep 30; wait_agent || { echo "$c: exit=$rc guest did not come back" >>"$OUT/summary.txt"; exit 1; }
    fi
    gps "Get-Content '$GUEST\\out\\$c.json' -Raw -ErrorAction SilentlyContinue" 60 >"$OUT/$c.json"
    # The probe leaves its request on disk before the call. It is still there when the guest died in it.
    died=no; [ "$(field "$OUT/$c.json" request_completed)" = True ] || died=yes
    after=$(last_bugcheck); after=${after:-0}
    if [ "$died" = yes ]; then
        # Windows writes the bugcheck event some time after the boot.
        for _ in $(seq 1 24); do
            [ "$after" != "$before" ] && break
            sleep 10; after=$(last_bugcheck); after=${after:-0}
        done
    fi
    bug=none
    if [ "$after" != "$before" ]; then
        gps '$e = Get-WinEvent -FilterHashtable @{LogName="System"; ProviderName="Microsoft-Windows-WER-SystemErrorReporting"; Id=1001} -MaxEvents 1; "record {0} at {1}" -f $e.RecordId, $e.TimeCreated.ToUniversalTime().ToString("o"); $e.Message' 60 >"$OUT/$c.bugcheck.txt"
        bug=$(grep -oE '0x[0-9a-fA-F]{8} \(0x[0-9a-fA-F]+, 0x[0-9a-fA-F]+, 0x[0-9a-fA-F]+, 0x[0-9a-fA-F]+\)' "$OUT/$c.bugcheck.txt" | head -1)
        bug=${bug:-recorded}
    fi
    f=$OUT/$c.json
    line="$c: exit=$rc result=$(field "$f" result) create_status=$(field "$f" create_status) handles=$(field "$f" allocation_handles) destroy_status=$(field "$f" destroy_status) pass=$(field "$f" pass) guest_died=$died bugcheck=$bug"
    [ "$c" != starve ] || line="$line succeeded=$(field "$f" succeeded) failed=$(field "$f" failed) statuses=$(field "$f" failure_statuses) seconds=$(field "$f" seconds)"
    note "$line"; echo "$line" >>"$OUT/summary.txt"
    if [ "$rc" != 0 ] || [ "$died" = yes ] || [ "$bug" != none ] || [ "$(field "$f" pass)" != True ]; then bad=1; fi
    if [ "$c" = starve ]; then
        gps 'verifier.exe /reset' 60 >>"$OUT/verifier.txt" 2>&1
        note "restarting the guest without Driver Verifier"
        restart_guest || exit 1
        gps 'verifier.exe /querysettings' 60 >>"$OUT/verifier.txt" 2>&1
    fi
done
gps "Remove-LocalUser -Name '$ACCOUNT'" 60
if [ "$bad" = 0 ]; then echo "PASS: every case passed and the guest never bugchecked" | tee -a "$OUT/summary.txt"; else echo "FAIL" | tee -a "$OUT/summary.txt"; fi
exit "$bad"
