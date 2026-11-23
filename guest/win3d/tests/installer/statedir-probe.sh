#!/usr/bin/env bash
# Guest proof that a standard user cannot get code run as SYSTEM, nor own the
# installer's state, through C:\ProgramData\vmlab-win3d
# (docs/acceptance/C753.md; finding F2 of docs/reviews/win3d-driver.md).
#
#   tests/installer/statedir-probe.sh <lab-dir> <vm> <evidence-dir> <step>...
#
# The VM runs already. The steps, in the order a run uses them:
#
#   user     make the standard user win3dprobe and show that it is not an
#            administrator
#   premake  as that user: make C:\ProgramData\vmlab-win3d (before an install)
#   attack   as that user, with a package installed: put another script in
#            place of the copy the startup tasks run, create state.json.tmp,
#            and create a file in bundle\. Each attempt is recorded as done
#            or refused.
#   check    restart the guest, wait for the startup tasks, and judge. Exit 1
#            when the user's script ran (it writes its identity to
#            C:\win3d-statedir-proof.txt) or the user owns state.json; exit 0
#            when neither happened.
#   state    the owner and the access list of the directory and of every
#            entry in it, into the evidence only
#
# The install between the steps is the caller's (scripts/install-into-vm.sh).
# Each step writes <evidence-dir>/<step>.txt. Set VMLAB to use another vmlab
# command.
set -euo pipefail
VMLAB=${VMLAB:-vmlab}
[ $# -ge 4 ] || { sed -n '2,27p' "$0" >&2; exit 2; }
LABDIR=$(cd "$1" && pwd)
VM=$2
mkdir -p "$3"; EV=$(cd "$3" && pwd)
shift 3
cd "$LABDIR"

U=win3dprobe
# A throwaway account in a lab VM; the password is not a secret.
PW='Pr0be-753-statedir'
ROOT='C:\ProgramData\vmlab-win3d'
PROOF='C:\win3d-statedir-proof.txt'

say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
psh() { powershell_as "" "$@"; }
usr() { powershell_as user "$@"; }
powershell_as() { # powershell_as ''|user 'powershell' [timeout]
    local who=() ; [ -z "$1" ] || who=(--user "$U" --password "$PW")
    "$VMLAB" exec --timeout "${3:-300}" "${who[@]}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$2" 2>&1 | tr -d '\r'
}
wait_agent() {
    for _ in $(seq 1 90); do
        "$VMLAB" exec --timeout 20 "$VM" -- cmd.exe /c ver >/dev/null 2>&1 && return 0
        sleep 10
    done
    echo "guest agent did not come back" >&2; return 1
}
# One attempt as the user: prints "<name>: done" or "<name>: refused: <why>".
attempt() { # attempt NAME 'powershell that throws when it is refused'
    usr "\$ErrorActionPreference = 'Stop'; try { $2; '$1: done' } catch { '$1: refused: ' + \$_.Exception.Message }" || true
}
tree_state() {
    psh "\$ErrorActionPreference = 'Continue'
        if (-not (Test-Path -LiteralPath '$ROOT')) { '$ROOT is absent'; return }
        foreach (\$p in @('$ROOT') + @(Get-ChildItem -LiteralPath '$ROOT' -Force -Recurse | ForEach-Object FullName)) {
            \$a = Get-Acl -LiteralPath \$p; '{0} | owner {1} | {2}' -f \$p, \$a.Owner, \$a.Sddl
        }" || true
}

for step in "$@"; do
    out=$EV/$step.txt
    case $step in
        user)
            say "user: the standard user $U"
            {
                psh "if (-not (Get-LocalUser -Name '$U' -ErrorAction SilentlyContinue)) { net.exe user '$U' '$PW' /add | Out-Null }
                    'administrator by group: ' + [bool](Get-LocalGroupMember -SID S-1-5-32-544 | Where-Object { \$_.Name -like '*\\$U' })"
                usr "'runs as ' + (whoami.exe); 'administrator by token: ' + ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)"
            } | tee "$out"
            grep -q 'administrator by group: False' "$out" && grep -q 'administrator by token: False' "$out" ||
                { echo "$U is not a standard user" >&2; exit 2; }
            ;;
        premake)
            say "premake: $U makes $ROOT"
            {
                attempt 'make the directory' "New-Item -ItemType Directory -Path '$ROOT' | Out-Null"
                tree_state
            } | tee "$out"
            ;;
        attack)
            say "attack: $U changes the state directory"
            {
                # In a directory the user owns, the script is not the user's
                # to write but is the user's to delete.
                attempt 'replace the script the startup tasks run' "
                    \$f = '$ROOT\\vmlab-win3d.ps1'; \$new = 'whoami.exe | Set-Content -LiteralPath $PROOF'
                    try { Set-Content -LiteralPath \$f -Value \$new } catch { Remove-Item -LiteralPath \$f -Force; Set-Content -LiteralPath \$f -Value \$new }"
                attempt 'create state.json.tmp' "Set-Content -LiteralPath '$ROOT\\state.json.tmp' -Value '{}'"
                attempt 'create a file in bundle' "Set-Content -LiteralPath '$ROOT\\bundle\\win3dprobe.txt' -Value 'x'"
                tree_state
            } | tee "$out"
            ;;
        state)
            tree_state >"$out"
            say "state: $(wc -l <"$out") entries in $out"
            ;;
        check)
            say "check: restart, then look for $PROOF and at the owner of state.json"
            psh 'shutdown.exe /r /t 0 /f' 60 >/dev/null || true
            sleep 45
            wait_agent
            # The repair task starts 10 s after boot and the resume task 20 s.
            sleep 150
            {
                psh "if (Test-Path -LiteralPath '$PROOF') { 'proof file: ' + ((Get-Content -LiteralPath '$PROOF') -join ' ') } else { 'proof file: absent' }
                    \$s = '$ROOT\\state.json'
                    if (Test-Path -LiteralPath \$s) { 'state.json owner: ' + (Get-Acl -LiteralPath \$s).GetOwner([Security.Principal.SecurityIdentifier]).Translate([Security.Principal.NTAccount]).Value } else { 'state.json owner: (no state.json)' }
                    Get-ScheduledTask -TaskName 'vmlab-win3d-*' -ErrorAction SilentlyContinue | ForEach-Object { 'task {0}: last result {1}' -f \$_.TaskName, (\$_ | Get-ScheduledTaskInfo).LastTaskResult }"
                tree_state
            } | tee "$out"
            bad=0
            grep -q '^proof file: absent' "$out" || { echo "FAIL: a script the standard user wrote ran at boot ($(grep '^proof file' "$out"))"; bad=1; }
            if grep '^state.json owner: ' "$out" | grep -qi "\\\\$U\$"; then echo "FAIL: the standard user owns state.json"; bad=1; fi
            [ "$bad" -eq 0 ] || exit 1
            echo "PASS: no script of the standard user ran at boot, and the user does not own state.json"
            ;;
        *) echo "unknown step $step" >&2; exit 2 ;;
    esac
done
