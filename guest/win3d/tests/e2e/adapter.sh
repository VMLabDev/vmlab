#!/usr/bin/env bash
# The adapter suite: does the device behave like a real video card? It checks
# the properties of docs/acceptance.md, "Adapter suite", that a test can check,
# in a lab where the package is installed and the autologon desktop is up
# (tests/e2e/run.sh --apps calls this after apps.sh; it also runs on its own
# against a lab kept with --keep).
#
#   tests/e2e/adapter.sh <lab-dir> <vm> <evidence-dir> [check...]
#
# Checks, in the order they run (all of them when none is named, or the ones
# in ADAPTER_CHECKS):
#
#   pointer       P1       the pointer is not part of the frame
#   pointer-shapes P1      the pointer's shapes reach the host (tests/e2e/pointer.sh)
#   pace          P2       vblank interval, present rate, flip timestamps, scan line
#   tear-flip     P3-P5    the tearing probe, flip-model presents
#   tear-blt      P3-P5    the tearing probe, presents of the Blt kind
#   duplication   P7       Desktop Duplication delivers frames
#   shared-texture P7      a shared render target of the duplication surface's shape still works
#   d2d           P10      the Direct2D coverage probe
#   d2d-tir       P10      what Direct2D draws to antialias, against WARP (tests/e2e/tir.sh)
#   svb-chrome    P10      the static vertex buffer page in Chrome
#   venc-chrome   P8       venc.html in Chrome: the GPU process survives
#   svb-edge, venc-edge    the same two pages in the guest's own Edge
#   chromium      P8       the GPU process loads the native d3d12.dll, and ends itself without (tests/e2e/chromium.sh)
#   wine          P10      Wine's Direct3D 9, 10 and 11 tests
#
# Each check prints pass, fail or not-run; tests/e2e/adapter/judge.py holds the
# thresholds and writes <evidence-dir>/adapter/adapter.json and adapter.txt.
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
[ $# -ge 3 ] || { sed -n '2,37p' "$0" >&2; exit 2; }
REPO=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$REPO/tests/e2e
A=$HERE/adapter
LABDIR=$(cd "$1" && pwd)
VM=$2
mkdir -p "$3/adapter"
EV=$(cd "$3/adapter" && pwd)   # absolute: the vmlab commands run from the lab directory
[ -n "$EV" ] && [ -d "$EV" ] || { echo "VML-88: the evidence directory is empty or missing" >&2; exit 2; }
shift 3
ALL="pointer pointer-shapes pace tear-flip tear-blt duplication shared-texture d2d d2d-tir svb-chrome venc-chrome svb-edge venc-edge chromium wine"
CHECKS=${*:-${ADAPTER_CHECKS:-$ALL}}
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

check_pointer() {
    note "pointer"
    local w h x y i pid shot=$STAGE/pointer.png
    # The probe fills the display for 30 s; the pointer is moved while it does.
    probe pointer 30 pointer_probe.exe &
    pid=$!
    for i in $(seq 1 60); do
        sleep 2
        capture "$shot" || continue
        python3 "$A/pointerframe.py" "$shot" 0 0 >"$STAGE/pointer-wait.json" 2>/dev/null || continue
        python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1]))["background"] > 0.9 else 1)' "$STAGE/pointer-wait.json" && break
        kill -0 "$pid" 2>/dev/null || break
    done
    if [ -f "$STAGE/pointer-wait.json" ]; then
        read -r w h < <(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["width"], d["height"])' "$STAGE/pointer-wait.json")
        i=0
        for p in "$((w / 4)) $((h / 4))" "$((w * 3 / 5)) $((h * 3 / 5))"; do
            i=$((i + 1))
            read -r x y <<<"$p"
            in_lab vmlab vm mouse-move "$VM" "$x" "$y" >/dev/null 2>&1 || note "  mouse-move failed"
            sleep 2
            capture "$STAGE/pointer-$i.png" && python3 "$A/pointerframe.py" "$STAGE/pointer-$i.png" "$x" "$y" >"$EV/pointer-frame-$i.json"
            keep_png "$STAGE/pointer-$i.png" "$EV/console-pointer-$i.png"
        done
    fi
    wait "$pid" && ran pointer || skip pointer "the pointer probe wrote no result"
}

check_pointer_shapes() {
    # The check of the hardware pointer (docs/pointer.md) is a script of its own; it
    # writes <evidence-dir>/pointer/ and its verdict there.
    note "pointer-shapes"
    local top rc
    top=$(dirname "$EV")
    [ -n "$top" ] && [ -d "$top" ] || { skip pointer-shapes "no evidence directory"; return; }
    "$HERE/pointer.sh" "$LABDIR" "$VM" "$top" >"$STAGE/pointer-shapes.out" 2>&1
    rc=$?
    grep -E '^(PASS|FAIL) ' "$STAGE/pointer-shapes.out" | while read -r l; do note "  $l"; done
    if [ -f "$top/pointer/pointer.json" ]; then
        cp "$top/pointer/pointer.json" "$EV/pointer-shapes.json"
        ran pointer-shapes
    else
        skip pointer-shapes "tests/e2e/pointer.sh ended with exit $rc and no verdict: $(tail -1 "$STAGE/pointer-shapes.out" | cut -c1-160)"
    fi
}

check_pace() {
    note "pace"
    probe pace 10 pace_probe.exe && ran pace || skip pace "the pace probe wrote no result"
}

vnc_sock() {
    # vmlab keeps a VM's sockets in <runtime>/labs/<lab>/vms/<vm>/. The runtime
    # directory is $XDG_RUNTIME_DIR/vmlab, or /tmp/vmlab-<uid>-<hash> when that
    # is too long for a socket path, and a long lab or VM name is hashed too
    # (src/paths.rs in the vmlab repository). The lab daemon's command line has
    # the name the lab runs under.
    local xdg=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} lab base l v
    lab=$(lab_name)
    [ -n "$lab" ] || return 1
    h() { printf %s "$1" | sha256sum | cut -c1-12; }
    for base in "$xdg/vmlab" "/tmp/vmlab-$(id -u)-$(h "$xdg")"; do
        for l in "$lab" "_$(h "$lab")"; do
            for v in "$VM" "_$(h "$VM")"; do
                [ -S "$base/labs/$l/vms/$v/vnc.sock" ] && { echo "$base/labs/$l/vms/$v/vnc.sock"; return 0; }
            done
        done
    done
    return 1
}

check_tear() { # check_tear flip|blt
    local mode=$1 name=tear-$1 sock gpid
    note "$name"
    sock=$(vnc_sock)
    [ -n "$sock" ] || { skip "$name" "found no vnc.sock for this lab's VM in the vmlab runtime directory"; return; }
    # Park the pointer away from the window, for a console that paints it into the frame.
    in_lab vmlab vm mouse-move "$VM" 1000 700 >/dev/null 2>&1
    python3 "$A/rfbgrab.py" "$sock" "$EV/$name.grab.json" --seconds $((TEAR_SECONDS + 300)) --stop "$STAGE/$name.stop" \
        --save-torn "$EV/$name-torn.png" >"$STAGE/$name.grab.out" 2>&1 &
    gpid=$!
    probe "$name" "$TEAR_SECONDS" tear_probe.exe --mode "$mode"
    local rc=$?
    : >"$STAGE/$name.stop"
    wait "$gpid"
    note "  $(tail -1 "$STAGE/$name.grab.out")"
    [ $rc = 0 ] && ran "$name" || skip "$name" "the tearing probe wrote no result"
}

check_duplication() {
    note "duplication"
    probe duplication 10 dupl_probe.exe && ran duplication || skip duplication "the duplication probe wrote no result"
}

check_shared_texture() {
    note "shared-texture"
    probe shared-texture 10 share_probe.exe && ran shared-texture || skip shared-texture "the shared-texture probe wrote no result"
}

check_d2d() {
    note "d2d"
    probe d2d 30 d2d_probe.exe --bmp "$R\\d2d.bmp" && ran d2d || skip d2d "the Direct2D probe wrote no result"
    pull d2d.bmp && magick "$EV/d2d.bmp" -strip "$EV/d2d.png" && rm -f "$EV/d2d.bmp"
}

check_d2d_tir() {
    # The check of target-independent rasterization (docs/d2d-coverage.md) is a
    # script of its own; it writes <evidence-dir>/tir/ and its verdict there.
    note "d2d-tir"
    local top rc
    top=$(dirname "$EV")
    [ -n "$top" ] && [ -d "$top" ] || { skip d2d-tir "no evidence directory"; return; }
    "$HERE/tir.sh" "$LABDIR" "$VM" "$top" >"$STAGE/d2d-tir.out" 2>&1
    rc=$?
    grep -E '^(pass|fail) ' "$STAGE/d2d-tir.out" | while read -r l; do note "  ${l:0:200}"; done
    if [ -s "$top/tir/tir.txt" ]; then
        cp "$top/tir/tir.txt" "$EV/d2d-tir.txt"
        ran d2d-tir
    else
        skip d2d-tir "tests/e2e/tir.sh ended with exit $rc and no verdict: $(tail -1 "$STAGE/d2d-tir.out" | cut -c1-160)"
    fi
}

CHROME=
stage_chrome() { # once: Chrome for Testing into C:\apps\chrome-win64
    [ -z "$CHROME" ] || return 0
    local zip
    CHROME=failed
    zip=$(fetch_pin chrome-for-testing) || return 1
    note "copying Chrome for Testing into the guest"
    in_lab vmlab cp "$zip" "$VM:C:/apps/chrome-win64.zip" || return 1
    gps 'Set-Location C:\apps; if (Test-Path chrome-win64) { Remove-Item chrome-win64 -Recurse -Force }; tar.exe -xf chrome-win64.zip; Remove-Item chrome-win64.zip; Test-Path C:\apps\chrome-win64\chrome.exe' 900 | grep -q True || return 1
    CHROME=ok
}

check_page() { # check_page NAME chrome|edge PAGE SECONDS [FEATURES]
    local name=$1 browser=$2 page=$3 secs=$4 features=${5:-} rc spid
    note "$name"
    if [ "$browser" = chrome ]; then
        stage_chrome
        [ "$CHROME" = ok ] || { skip "$name" "Chrome for Testing did not download or unpack"; return; }
    fi
    (sleep $((secs - 6)); capture "$STAGE/$name.png") &
    spid=$!
    in_lab vmlab exec --timeout $((secs + 400)) "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \
        "$G\\guest\\Run-Page.ps1" -Browser "$browser" -Page "$page" -Name "$name" -OutDir "$R" -Seconds "$secs" \
        ${features:+-Features "$features"} >"$STAGE/$name.out" 2>&1
    rc=$?
    wait "$spid"
    keep_png "$STAGE/$name.png" "$EV/console-$name.png"
    pull "$name.page.json"
    note "  Run-Page exit $rc"
    if [ $rc = 2 ]; then skip "$name" "$browser is not installed in the guest"
    elif [ -f "$EV/$name.page.json" ]; then ran "$name"
    else skip "$name" "Run-Page.ps1 wrote no result"; fi
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

check_wine() {
    note "wine"
    local exe base rc n=0 t0 exits
    if ! compgen -G "$OUT/winetests/x64/*.exe" >/dev/null; then
        skip wine "Wine's tests were not built (the pin did not download, or the build failed)"
        return
    fi
    in_lab vmlab cp "$OUT/winetests/x64" "$VM:C:/win3d/adapter/winetests" || { skip wine "the tests did not copy into the guest"; return; }
    for exe in "$OUT"/winetests/x64/*.exe; do basename "$exe" .exe; done >"$EV/wine.programs"
    for exe in "$OUT"/winetests/x64/*.exe; do
        base=$(basename "$exe" .exe)
        if ! guest_up && ! vm_restart; then note "  the guest does not answer; $base not run"; continue; fi
        note "  $base (at most $WINE_TIMEOUT s)"
        t0=$(date +%s)
        exits=$(hv_exits)
        in_lab vmlab exec --timeout $((WINE_TIMEOUT + 400)) "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \
            "$G\\guest\\Run-WineTest.ps1" -Path "$G\\winetests\\$base.exe" -OutDir "$R" -TimeoutSec "$WINE_TIMEOUT" \
            >"$STAGE/wine-$base.out" 2>&1
        rc=$?
        if pull "wine-$base.wine.json"; then
            pull "wine-$base.failures.txt"
        elif ! guest_up; then
            # The program took the VM with it: say so in its place.
            python3 -c 'import json,sys; json.dump({"name": sys.argv[1], "completed": False, "vm_lost": True, "duration_s": int(sys.argv[2]), "hypervisor": sys.argv[3]}, open(sys.argv[4], "w"), indent=2)' \
                "$base" "$(($(date +%s) - t0))" "$(hv_exit "${exits:-0}")" "$EV/wine-$base.wine.json"
        fi
        [ -f "$EV/wine-$base.wine.json" ] && n=$((n + 1))
        note "    exit $rc: $(python3 -c 'import json,sys; d=json.load(open(sys.argv[1],encoding="utf-8-sig")); print("the VM stopped: " + d["hypervisor"] if d.get("vm_lost") else "%s executed, %s failures, %s skipped, %s s%s%s" % (d["executed"], d["failures"], d["skipped"], d["duration_s"], ", timed out" if d["timed_out"] else "", ", unhandled exception" if d["crashed"] else ""))' "$EV/wine-$base.wine.json" 2>/dev/null || echo 'no result')"
    done
    [ $n -gt 0 ] && ran wine || skip wine "no Wine test program wrote a result"
}
check_chromium() {
    # The check of the native d3d12.dll preload (docs/chromium.md) is a script of its
    # own; it writes <evidence-dir>/chromium/ and its verdict there. It restarts the VM.
    note "chromium"
    local top rc
    top=$(dirname "$EV")
    [ -n "$top" ] && [ -d "$top" ] || { skip chromium "no evidence directory"; return; }
    "$HERE/chromium.sh" "$LABDIR" "$VM" "$top" >"$STAGE/chromium.out" 2>&1
    rc=$?
    grep -E '^(PASS|FAIL) ' "$STAGE/chromium.out" | while read -r l; do note "  ${l:0:200}"; done
    if [ -f "$top/chromium/chromium.json" ]; then
        cp "$top/chromium/chromium.json" "$EV/chromium.json"
        ran chromium
    else
        skip chromium "tests/e2e/chromium.sh ended with exit $rc and no verdict: $(tail -1 "$STAGE/chromium.out" | cut -c1-160)"
    fi
}

note "adapter suite: $CHECKS"
if ! guest_up; then
    note "the guest does not answer"
    for c in $CHECKS; do skip "$c" "the guest did not answer when the suite started"; done
elif ! stage; then
    for c in $CHECKS; do skip "$c" "staging failed (see the run log)"; done
else
    for c in $ALL; do
        wanted "$c" || continue
        if ! guest_up; then skip "$c" "the guest stopped answering before this check"; continue; fi
        case $c in
            pointer) check_pointer ;;
            pointer-shapes) check_pointer_shapes ;;
            pace) check_pace ;;
            tear-flip) check_tear flip ;;
            tear-blt) check_tear blt ;;
            duplication) check_duplication ;;
            shared-texture) check_shared_texture ;;
            d2d) check_d2d ;;
            d2d-tir) check_d2d_tir ;;
            svb-chrome) check_page svb-chrome chrome svb.html 25 ;;
            venc-chrome) check_page venc-chrome chrome venc.html 45 D3D12VideoEncodeAccelerator,D3D12VideoDecoder ;;
            svb-edge) check_page svb-edge edge svb.html 25 ;;
            venc-edge) check_page venc-edge edge venc.html 45 D3D12VideoEncodeAccelerator,D3D12VideoDecoder ;;
            chromium) check_chromium ;;
            wine) check_wine ;;
        esac
    done
fi
for c in $ALL; do
    wanted "$c" || echo "not-run: not selected for this run" >"$EV/$c.status"
done
find "$STAGE" -name '*.png' -delete
python3 "$A/judge.py" "$EV"
