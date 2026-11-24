#!/usr/bin/env bash
# Runs the pipe probe (probes/pipe) in a Windows VM as a standard user: the
# messages of card #757 against the pipe classes of the two helpers of the
# kernel driver package (docs/reviews/win3d-driver.md F9).
#
#   tests/kmd/pipe-probe.sh <lab-dir> <vm> <out-dir> <before|after> [case...]
#
# <lab-dir> holds the lab's vmlab.wcl and <vm> is the VM name there. The VM
# runs. The script copies out/probes-extra/x64/pipe_probe_<build>.exe
# (build/probes/build.sh pipe) into the guest: "before" is linked with
# pipe.cpp of the driver source without the patch of the card, "after" with
# it. It makes the local account pipe-probe, a member of Users only, with a
# random password, and runs one case per process as that account through
# `vmlab exec --user`. With no case names it runs all of them.
#
# The probe works on a pipe of its own and is all user mode: it does not
# depend on the driver the guest runs, and it cannot bugcheck the guest. The
# case "access" only tries to open the pipes of the installed service.
#
# <out-dir> gets <build>-<case>.json and <build>-summary.txt.
# Exit status: 0 when every case passed, 1 otherwise, 2 on usage.
# check-shell-paths: no-errexit a case that fails or kills the probe is a result to record, and the next case still runs (VML-88).
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
if [ $# -lt 4 ]; then
    sed -n '2,/^# check-shell-paths:/{ /^# check-shell-paths:/!p; }' "$0" >&2
    exit 2
fi
LABDIR=$(cd "$1" && pwd)
VM=$2
mkdir -p "$3"
OUT=$(cd "$3" && pwd)
[ -n "$OUT" ] && [ "$OUT" != / ] || { echo "VML-88: the output directory is empty or /" >&2; exit 2; }
BUILD=$4
case $BUILD in before|after) ;; *) echo "the build is before or after, not '$BUILD'" >&2; exit 2 ;; esac
shift 4
CASES=${*:-server-command server-long server-full service-thread client-command client-long client-long-message-mode access}
PROBE=$REPO/out/probes-extra/x64/pipe_probe_$BUILD.exe
[ -f "$PROBE" ] || { echo "no $PROBE; run build/probes/build.sh pipe" >&2; exit 2; }
VMLAB=${VMLAB:-vmlab}
ACCOUNT=pipe-probe
GUEST='C:\win3d\pipe'

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { # gps 'powershell' [timeout]: run PowerShell as SYSTEM in the guest
    in_lab "$VMLAB" exec --user SYSTEM --timeout "${2:-120}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'
}
field() { # field FILE NAME: a value of the probe's JSON
    python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1], encoding="utf-8-sig")).get(sys.argv[2]))
except Exception: print("unreadable")' "$1" "$2"
}

for _ in $(seq 1 90); do
    in_lab "$VMLAB" exec --user SYSTEM --timeout 20 "$VM" -- cmd.exe /c ver >/dev/null 2>&1 && break
    sleep 10
done
gps "New-Item -ItemType Directory -Force '$GUEST\\out' | Out-Null" 60 || exit 1
in_lab "$VMLAB" cp "$PROBE" "$VM:C:/win3d/pipe/pipe_probe_$BUILD.exe" || exit 1
sha256sum "$PROBE" | sed "s|$REPO/||" >"$OUT/$BUILD-probe.txt"

# A standard user. The password is random and stays in this process.
PASSWORD="Pp-$(head -c 18 /dev/urandom | base64 | tr -d '/+=')-9z"
gps "\$p = ConvertTo-SecureString '$PASSWORD' -AsPlainText -Force
    if (Get-LocalUser -Name '$ACCOUNT' -ErrorAction SilentlyContinue) { Set-LocalUser -Name '$ACCOUNT' -Password \$p }
    else { New-LocalUser -Name '$ACCOUNT' -Password \$p -PasswordNeverExpires | Out-Null }
    if (-not (Get-LocalGroupMember -SID S-1-5-32-545 | Where-Object Name -like '*\\$ACCOUNT')) { Add-LocalGroupMember -SID S-1-5-32-545 -Member '$ACCOUNT' }
    icacls '$GUEST\\out' /grant '${ACCOUNT}:(OI)(CI)M' | Out-Null" 120 || exit 1
in_lab "$VMLAB" exec --user "$ACCOUNT" --password "$PASSWORD" --timeout 60 "$VM" -- cmd.exe /c "whoami /groups /fo csv /nh" 2>&1 | tr -d '\r' >"$OUT/$BUILD-whoami.txt"
if grep -q 'S-1-5-32-544' "$OUT/$BUILD-whoami.txt" || ! grep -q 'S-1-5-32-545' "$OUT/$BUILD-whoami.txt"; then
    note "the account is not a standard user; see $BUILD-whoami.txt"; exit 1
fi

bad=0
: >"$OUT/$BUILD-summary.txt"
for c in $CASES; do
    gps "Remove-Item '$GUEST\\out\\$BUILD-$c.json' -ErrorAction SilentlyContinue" 60
    rc=0
    in_lab "$VMLAB" exec --user "$ACCOUNT" --password "$PASSWORD" --timeout 90 "$VM" -- \
        "$GUEST\\pipe_probe_$BUILD.exe" --case "$c" --out "$GUEST\\out\\$BUILD-$c.json" >"$OUT/$BUILD-$c.stdout.txt" 2>&1 || rc=$?
    # The file on disk: it says "requesting" when the process died in the read.
    gps "Get-Content '$GUEST\\out\\$BUILD-$c.json' -Raw -ErrorAction SilentlyContinue" 60 >"$OUT/$BUILD-$c.json"
    f=$OUT/$BUILD-$c.json
    line="$c: exit=$rc result=$(field "$f" result) read=$(field "$f" read_returned) text_units=$(field "$f" text_units) past_buffer=$(field "$f" bytes_written_past_buffer) second_ok=$(field "$f" second_message_ok) heap_valid=$(field "$f" heap_valid) pass=$(field "$f" pass)"
    [ "$c" != access ] || line="$c: exit=$rc pipes=$(field "$f" pipes)"
    note "$line"; echo "$line" >>"$OUT/$BUILD-summary.txt"
    if [ "$rc" != 0 ] || [ "$(field "$f" pass)" != True ]; then bad=1; fi
done
gps "Remove-LocalUser -Name '$ACCOUNT'" 60
if [ "$bad" = 0 ]; then echo "PASS: every case passed" | tee -a "$OUT/$BUILD-summary.txt"; else echo "FAIL" | tee -a "$OUT/$BUILD-summary.txt"; fi
exit "$bad"
