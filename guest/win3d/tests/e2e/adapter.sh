#!/usr/bin/env bash
# The adapter suite: does the device behave like a real video card? It checks
# the properties of docs/acceptance.md, "Adapter suite", that a test can check,
# in a lab where the package is installed and the autologon desktop is up
# (tests/e2e/run.sh --apps calls this after apps.sh; it also runs on its own
# against a lab kept with --keep).
#
#   tests/e2e/adapter.sh <lab-dir> <vm> <evidence-dir> [check...]
#
# Check names run in the order registered by adapter/checks/*.sh.
# With no names on the command line, run ADAPTER_CHECKS or all checks.
#
# Each check prints pass, fail or not-run. The verdicts in adapter/checks/
# feed adapter/judge.py, which writes <evidence-dir>/adapter/adapter.json and adapter.txt.
# What the probes measured, the console captures and the Wine tests' failed
# lines are next to them. Chrome is Chrome for Testing and the Wine tests are
# built from Wine's source; both are pinned in tests/e2e/PINS.
#
# ADAPTER_WINE_TIMEOUT  seconds one Wine test program may run (900)
# ADAPTER_TEAR_SECONDS  seconds each tearing run lasts (40)
#
# Exit status: 0 when no check failed, 1 when one did, 2 on usage. A check
# that did not run is not a failure.
# check-shell-paths: no-errexit each check records its own result and the suite goes on to the next (VML-88).
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$REPO/tests/e2e
A=$HERE/adapter

# Source each check before running any of them. C locale fixes the file order.
ALL=()
load_checks() {
    local LC_ALL=C check_file
    for check_file in "$A"/checks/[0-9]*.sh; do
        source "$check_file" || return
    done
}
load_checks || exit 2
if [ $# -lt 3 ]; then
    sed -n '2,/^# check-shell-paths:/{ /^# check-shell-paths:/!p; }' "$0" >&2
    printf 'Checks: %s\n' "${ALL[*]}" >&2
    exit 2
fi
LABDIR=$(cd "$1" && pwd)
VM=$2
mkdir -p "$3/adapter"
EV=$(cd "$3/adapter" && pwd)   # absolute: the vmlab commands run from the lab directory
[ -n "$EV" ] && [ -d "$EV" ] || { echo "VML-88: the evidence directory is empty or missing" >&2; exit 2; }
shift 3
CHECKS=${*:-${ADAPTER_CHECKS:-${ALL[*]}}}
CACHE=$REPO/.scratch/e2e/cache
STAGE=$REPO/.scratch/e2e/adapter/$(basename "$(dirname "$EV")")   # per evidence dir, so parallel runs do not share files
OUT=$REPO/out/adapter
G='C:\win3d\adapter'
R='C:\win3d\results\adapter'
WINE_TIMEOUT=${ADAPTER_WINE_TIMEOUT:-900}
TEAR_SECONDS=${ADAPTER_TEAR_SECONDS:-40}
mkdir -p "$EV" "$STAGE"
find "$EV" -mindepth 1 -delete
find "$STAGE" -mindepth 1 -delete

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { in_lab vmlab exec --timeout "${2:-600}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'; }
guest_up() { in_lab vmlab exec --timeout 30 "$VM" -- cmd.exe /c ver >/dev/null 2>&1; }
wanted() { case " $CHECKS " in *" $1 "*) return 0 ;; esac; return 1; }
# The state of a check for judge.py: "ran", or "not-run: <why>".
ran() { echo ran >"$EV/$1.status"; }
skip() { echo "not-run: $2" >"$EV/$1.status"; note "$1 not run: $2"; }
pull() { in_lab vmlab cp "$VM:C:/win3d/results/adapter/$1" "$EV/${2:-$1}" >/dev/null 2>&1; }
capture() { in_lab vmlab vm screenshot "$VM" "$1" >/dev/null 2>&1; }
keep_png() { [ -f "$1" ] && magick "$1" -resize 50% -strip -colors 256 "$2"; }
fetch_pin() {
    local name=$1 sha url f
    read -r _ sha url < <(grep "^$name " "$HERE/PINS") || return 1
    f=$CACHE/$(basename "$url")
    mkdir -p "$CACHE"
    if [ ! -f "$f" ]; then curl -sSfLo "$f.part.$$" "$url" && mv "$f.part.$$" "$f" || return 1; fi
    echo "$sha  $f" | sha256sum -c --quiet - >&2 || { note "sha256 mismatch for $name"; return 1; }
    echo "$f"
}

probe() { # probe NAME SECONDS EXE [ARGS...]: run one of the suite's probes in the user's session
    local name=$1 secs=$2 exe=$3 arglist=
    shift 3
    [ $# -gt 0 ] && arglist="-ArgumentList $(printf "'%s'," "$@" | sed 's/,$//')"
    in_lab vmlab exec --timeout $((secs + 400)) "$VM" -- powershell.exe -NoProfile -ExecutionPolicy Bypass -Command \
        "& C:\\win3d\\guest\\Run-Probe.ps1 -Path '$G\\x64\\$exe' $arglist -NoModules -Name $name -OutDir '$R' -Seconds $secs; exit \$LASTEXITCODE" \
        >"$STAGE/$name.out" 2>&1
    local rc=$?
    pull "$name.json"
    pull "$name.run.json"
    note "  $exe exit $rc"
    [ -f "$EV/$name.json" ]
}

stage() {
    local tarball=
    if wanted wine; then
        tarball=$(fetch_pin wine) || { note "the Wine source did not download"; tarball=; }
    fi
    note "building the probes${tarball:+ and the Wine Direct3D tests}"
    "$A/build.sh" $tarball >"$STAGE/build.log" 2>&1 || { tail -5 "$STAGE/build.log"; note "build failed"; return 1; }
    gps "New-Item -ItemType Directory -Force '$R' | Out-Null" 120 || return 1
    in_lab vmlab cp "$REPO/tests/guest" "$VM:C:/win3d/guest" || return 1
    in_lab vmlab cp "$A/guest" "$VM:C:/win3d/adapter/guest" || return 1
    in_lab vmlab cp "$A/pages" "$VM:C:/win3d/adapter/pages" || return 1
    in_lab vmlab cp "$OUT/x64" "$VM:C:/win3d/adapter/x64" || return 1
}

lab_name() { # the name the lab runs under: a lab in a linked git worktree is <name>-<instance>
    pgrep -af -- "__labd --lab [^ ]* --root $LABDIR( |\$)" | sed -n 's/.*__labd --lab \([^ ]*\) .*/\1/p' | head -1
}

RESTARTS=0
vm_restart() { # start the VM again after a test ended it, and wait for the user's desktop. At most 3 times a run.
    [ "$RESTARTS" -lt 3 ] || { note "  the VM is down and was restarted 3 times already; not again"; return 1; }
    RESTARTS=$((RESTARTS + 1))
    note "  the VM is down; starting it again (restart $RESTARTS)"
    local i r
    # Right after the hypervisor ended, the lab daemon can still count the VM
    # as running and refuse the start; ask until it takes it.
    for i in $(seq 1 12); do
        in_lab vmlab vm start "$VM" >"$STAGE/restart-$RESTARTS.out" 2>&1 && break
        sleep 5
    done
    for i in $(seq 1 60); do
        r=$(gps '$e = Get-Process explorer -ErrorAction SilentlyContinue | Where-Object SessionId -gt 0; $up = ((Get-Date) - (Get-CimInstance Win32_OperatingSystem).LastBootUpTime).TotalSeconds; "{0} {1}" -f [bool]$e, [int]$up' 60 2>/dev/null | tail -1)
        case $r in "True "*) [ "${r#True }" -ge 90 ] && return 0 ;; esac
        # Still stopped after a minute: the start did not take.
        if [ "$i" = 6 ] && in_lab vmlab status 2>/dev/null | grep -Eq "^ *$VM +vm +stopped"; then
            in_lab vmlab vm start "$VM" >>"$STAGE/restart-$RESTARTS.out" 2>&1
        fi
        sleep 10
    done
    note "  the desktop did not come back"
    return 1
}

hv_log() { echo "${XDG_STATE_HOME:-$HOME/.local/state}/vmlab/labs/$(lab_name)/vms/$VM/vmlab-hv.log"; }
hv_exits() { # how many exit lines the log has: it outlives a run, and has those of earlier ones
    local n
    n=$(grep -ac '"process exit' "$(hv_log)" 2>/dev/null)
    echo "${n:-0}"
}
hv_exit() { # hv_exit COUNT: what vmlab-hv logged about its own end since the log had COUNT exit lines, for the note of a test that ended the VM
    local log i before=${1:-0}
    log=$(hv_log)
    [ -f "$log" ] || return 0
    # The exit line comes a few seconds after the first error.
    for i in 1 2 3 4 5 6 7 8 9 10; do
        [ "$(hv_exits)" -gt "$before" ] && break
        sleep 2
    done
    if [ "$(hv_exits)" -gt "$before" ]; then grep -a '"process exit' "$log"; else grep -a '"level":"ERROR"' "$log"; fi | tail -1 | python3 -c 'import json,sys
for line in sys.stdin:
    try: print(json.loads(line)["message"][:200])
    except (ValueError, KeyError): pass'
}

note "adapter suite: $CHECKS"
if ! guest_up; then
    note "the guest does not answer"
    for c in $CHECKS; do skip "$c" "the guest did not answer when the suite started"; done
elif ! stage; then
    for c in $CHECKS; do skip "$c" "staging failed (see the run log)"; done
else
    for c in "${ALL[@]}"; do
        wanted "$c" || continue
        if ! guest_up; then skip "$c" "the guest stopped answering before this check"; continue; fi
        "check_${c//-/_}"
    done
fi
for c in "${ALL[@]}"; do
    wanted "$c" || echo "not-run: not selected for this run" >"$EV/$c.status"
done
find "$STAGE" -name '*.png' -delete
python3 "$A/judge.py" "$EV"
