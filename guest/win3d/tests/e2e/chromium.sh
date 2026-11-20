#!/usr/bin/env bash
# A Chromium GPU process keeps running with the Direct3D 12 bridge installed
# (patches/mesa 0096, docs/chromium.md, GitHub #238), checked in a lab where
# the package is installed and the autologon desktop is up (a lab kept with
# tests/e2e/run.sh --keep). tests/e2e/adapter.sh runs it as its check
# "chromium".
#
#   tests/e2e/chromium.sh <lab-dir> <vm> <evidence-dir>
#
# tests/guest/Test-ChromiumGpu.ps1 opens tests/e2e/adapter/pages/venc.html in the
# user's session: the page asks for a hardware video encoder, which makes the
# GPU process load d3d12.dll, then tries WebGL every 4 s for 20 s. Rows:
#
#   chrome, edge                  the page in Chrome for Testing (tests/e2e/PINS)
#                                 and in the guest's own Edge, both with the
#                                 features that make the GPU process load
#                                 d3d12.dll (Chrome's field trials turn them on)
#   chrome-control, edge-control  the same with the preload off in yttrium.ini.
#                                 The GPU process must end itself here, or the
#                                 rows above have proved nothing
#   native                        the package's own d3d12_probe.exe: a native
#                                 D3D12CreateDevice still gets the bridge
#
# A browser row passes when WebGL is on the adapter before the query and in
# all five tries after it, the profile has no crash dump, one GPU process ran
# for the whole page, and every d3d12.dll it loaded is the native copy.
# Writes <evidence-dir>/chromium/: one JSON per row and chromium.json, the
# verdict. Exit status: 0 when all rows pass, 1 otherwise, 2 on usage.
#
# The check restarts the VM at its start and again before the controls.
# vmlab-hv does not give back every file descriptor a guest GPU context used.
# After the probes of tests/e2e/run.sh it held 825 of its 1024 and the VM
# stopped in the fourth browser start ("Too many open files"). From one
# restart the five rows ended at 863 on AMD1 and at 910 on Nvidia2.
# check-shell-paths: no-errexit each row records its rc and the judge decides; the ini must be put back (VML-88).
set -uo pipefail
[ $# -eq 3 ] || { sed -n '2,34p' "$0" >&2; exit 2; }
REPO=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$REPO/tests/e2e
LABDIR=$(cd "$1" && pwd)
VM=$2
mkdir -p "$3/chromium"
EV=$(cd "$3/chromium" && pwd)   # absolute: the vmlab commands run from the lab directory
STAGE=$REPO/.scratch/e2e/chromium/$(basename "$3")
CACHE=$REPO/.scratch/e2e/cache
FEATURES=D3D12VideoEncodeAccelerator,D3D12VideoDecoder
# The page needs 21 s. The first start of Chrome for Testing in a guest took
# 26 s to show its GPU process.
SECONDS_PER_PAGE=90
G='C:\win3d\guest'
R='C:\win3d\results\chromium'
INI='C:\ProgramData\Yttrium\yttrium.ini'
OPTION=D3D10UMD_YTTRIUM_NATIVE_D3D12_PRELOAD

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { in_lab vmlab exec --timeout "${2:-600}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'; }
gfile() { # gfile timeout script.ps1 args...: run a guest script file as SYSTEM
    local t=$1; shift
    in_lab vmlab exec --timeout "$t" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$@" | tr -d '\r'
}
fetch_pin() {
    local name=$1 sha url f
    read -r _ sha url < <(grep "^$name " "$HERE/PINS") || return 1
    f=$CACHE/$(basename "$url")
    mkdir -p "$CACHE"
    if [ ! -f "$f" ]; then curl -sSfLo "$f.part.$$" "$url" && mv "$f.part.$$" "$f" || return 1; fi
    echo "$sha  $f" | sha256sum -c --quiet - >&2 || { note "sha256 mismatch for $name"; return 1; }
    echo "$f"
}

[ -n "$EV" ] && [ -d "$EV" ] && [ "$EV" != / ] || { echo "VML-88: the evidence directory is empty or missing" >&2; exit 2; }
[ -n "$STAGE" ] && [ "$STAGE" != / ] || { echo "VML-88: STAGE is empty or /" >&2; exit 2; }
mkdir -p "$EV" "$STAGE"
find "$EV" -mindepth 1 -delete
find "$STAGE" -mindepth 1 -type f -delete

restart_vm() {
    note "restarting $VM"
    in_lab vmlab vm restart "$VM" || return 1
    # The agent first, then the autologon user: Run-Probe.ps1 needs the desktop.
    for _ in $(seq 1 60); do
        [ -n "$(gps '(Get-CimInstance Win32_ComputerSystem).UserName' 30 2>/dev/null | tr -d '[:space:]')" ] && break
        sleep 10
    done
    sleep 30
}

note "chromium check in $LABDIR"
restart_vm || exit 1
gps "New-Item -ItemType Directory -Force '$G', 'C:\\win3d\\chromium', 'C:\\apps' | Out-Null; if (Test-Path '$R') { Remove-Item '$R' -Recurse -Force }" 120 || exit 1
for f in Run-Probe.ps1 Test-ChromiumGpu.ps1; do
    in_lab vmlab cp "$REPO/tests/guest/$f" "$VM:C:/win3d/guest/$f" || exit 1
done
in_lab vmlab cp "$HERE/adapter/pages/venc.html" "$VM:C:/win3d/chromium/venc.html" || exit 1
# What the installer left: the bridge, the native file and its copy, per architecture.
gps 'foreach ($d in "System32", "SysWOW64") { foreach ($n in "d3d12.dll", "vmlab-win3d\d3d12-native.dll", "vmlab-win3d\native\d3d12.dll") {
        $p = Join-Path (Join-Path $env:SystemRoot $d) $n
        if (Test-Path -LiteralPath $p) { "{0}  {1}  {2}" -f (Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash, [Diagnostics.FileVersionInfo]::GetVersionInfo($p).FileVersion, $p } else { "missing  $p" } } }' 120 | tee "$EV/files.txt"

CHROME=missing
if gps 'Test-Path C:\apps\chrome-win64\chrome.exe' 60 | grep -q True; then
    CHROME=ok
elif zip=$(fetch_pin chrome-for-testing); then
    note "copying Chrome for Testing into the guest"
    in_lab vmlab cp "$zip" "$VM:C:/apps/chrome-win64.zip" &&
        gps 'Set-Location C:\apps; tar.exe -xf chrome-win64.zip; Remove-Item chrome-win64.zip; Test-Path C:\apps\chrome-win64\chrome.exe' 900 | grep -q True && CHROME=ok
fi
[ "$CHROME" = ok ] || note "Chrome for Testing did not download or unpack: the chrome rows will fail"

page() { # page NAME chrome|edge
    local name=$1 browser=$2
    note "$name"
    gfile $((SECONDS_PER_PAGE + 400)) "$G\\Test-ChromiumGpu.ps1" -Browser "$browser" -Page 'C:\win3d\chromium\venc.html' -Name "$name" \
        -OutDir "$R" -Seconds "$SECONDS_PER_PAGE" -Features "$FEATURES" >"$STAGE/$name.out" 2>&1
    note "  Test-ChromiumGpu exit $?"
    in_lab vmlab cp "$VM:C:/win3d/results/chromium/$name.browser.json" "$EV/$name.json" >/dev/null 2>&1 || note "  no result file"
}
# The UMD reads the option when a GPU process opens the adapter, so the file
# only has to be in place while the control browsers run.
preload_off() {
    gps "\$old = if (Test-Path '$INI') { @(Get-Content '$INI' | Where-Object { \$_ -notmatch '^$OPTION=' }) } else { \$null }
         New-Item -ItemType Directory -Force (Split-Path '$INI') | Out-Null
         if (\$null -ne \$old) { Copy-Item '$INI' '$INI.win3d-chromium' -Force }
         Set-Content '$INI' -Value (@(\$old | Where-Object { \$null -ne \$_ }) + '$OPTION=0') -Encoding ASCII; 'preload off'" 120
}
preload_on() {
    gps "if (Test-Path '$INI.win3d-chromium') { Move-Item '$INI.win3d-chromium' '$INI' -Force } else { Remove-Item '$INI' -ErrorAction SilentlyContinue }; 'preload on'" 120
}

page chrome chrome
page edge edge

note "native"
gfile 900 "$G\\Run-Probe.ps1" -Path 'C:\win3d\probes\x64\d3d12_probe.exe' -Name native -OutDir "$R" -Seconds 10 >"$STAGE/native.out" 2>&1
note "  Run-Probe exit $?"
in_lab vmlab cp "$VM:C:/win3d/results/chromium/native.run.json" "$EV/native.json" >/dev/null 2>&1 || note "  no result file"

# The controls last: a GPU process that ends itself again and again is the
# fault, and what it does to the guest must not cost the rows above.
restart_vm || note "could not restart $VM"
if preload_off; then
    page chrome-control chrome
    page edge-control edge
fi
preload_on || note "could not put $INI back"

python3 - "$EV" <<'PYEOF'
import json, os, re, sys
ev = sys.argv[1]
SOFTWARE = re.compile(r'swiftshader|basic render|llvmpipe|warp|software', re.I)
NATIVE = re.compile(r'\\vmlab-win3d\\native\\d3d12\.dll$', re.I)
rows = []

def load(name):
    try:
        with open(os.path.join(ev, name + '.json'), encoding='utf-8-sig') as f:
            return json.load(f)
    except (OSError, ValueError):
        return None

def hardware(s):
    return bool(s) and s != 'NO WEBGL' and not SOFTWARE.search(s)

def facts(d):
    r = d.get('result') or {}
    gpus = d.get('gpu_processes') or []
    if len(gpus) == 1 and isinstance(gpus[0].get('value'), list):
        gpus = gpus[0]['value']  # an array that Windows PowerShell 5.1 wrapped
    return {
        'before': r.get('before'), 'after': r.get('after') or [],
        'dumps': d.get('crash_dumps') or 0, 'gpu_processes': len(gpus),
        'd3d12': sorted({p for g in gpus for p in (g.get('d3d12') or [])}),
        'signed_only': sorted({str(g.get('microsoft_signed_only')) for g in gpus}),
        'encoder': r.get('encoder'), 'webgpu': r.get('webgpu'),
    }

def describe(d, f):
    label = 'Chrome for Testing' if d.get('browser') == 'chrome' else 'Edge'
    return (f"{label} {d.get('version')}: WebGL before the query {f['before']!r}, on the adapter in "
            f"{sum(hardware(a) for a in f['after'])} of {len(f['after'])} tries after it, {f['dumps']} crash dumps, "
            f"{f['gpu_processes']} GPU processes, d3d12.dll loaded from {f['d3d12'] or 'nowhere'}, "
            f"MicrosoftSignedOnly {f['signed_only']}; encoder {f['encoder']}; WebGPU {f['webgpu']}")

for browser in ('chrome', 'edge'):
    d = load(browser)
    if not d or not d.get('reported'):
        rows.append((browser, 'fail', 'the page did not report' if d else 'no result file'))
    else:
        f = facts(d)
        ok = (hardware(f['before']) and len(f['after']) == 5 and all(hardware(a) for a in f['after'])
              and not f['dumps'] and f['gpu_processes'] == 1 and all(NATIVE.search(p) for p in f['d3d12']))
        rows.append((browser, 'pass' if ok else 'fail', describe(d, f)))
    c = load(browser + '-control')
    if not c:
        rows.append((browser + '-control', 'fail', 'no result file'))
    else:
        f = facts(c)
        # With the preload off the GPU process must end itself: a dump, or a second GPU process.
        fault = f['dumps'] > 0 or f['gpu_processes'] > 1
        rows.append((browser + '-control', 'pass' if fault else 'fail',
                     ('the fault shows with the preload off: ' if fault
                      else 'no fault with the preload off, so the row above proves nothing: ') + describe(c, f)))

n = load('native')
if not n:
    rows.append(('native', 'fail', 'no result file'))
else:
    mods = n.get('graphics_modules') or []
    bridge = (any(re.search(r'\\system32\\d3d12\.dll', m, re.I) for m in mods)
              and any(re.search(r'\\vmlab-win3d\\vkd3dcore\.dll', m, re.I) for m in mods))
    preloaded = any(re.search(r'\\native\\d3d12\.dll', m, re.I) for m in mods)
    ok = n.get('exit_code') == 0 and n.get('probe_result') == 'pass' and bridge and not preloaded and not n.get('software')
    rows.append(('native', 'pass' if ok else 'fail',
                 f"d3d12_probe.exe exit {n.get('exit_code')}, result {n.get('probe_result')!r}, adapter {n.get('adapter')!r}, "
                 f"readback {n.get('readback_ok')}; bridge and vkd3dcore.dll loaded: {bridge}; native copy loaded: {preloaded}"))

passed = sum(1 for r in rows if r[1] == 'pass')
with open(os.path.join(ev, 'chromium.json'), 'w') as f:
    json.dump({'passed': passed, 'total': len(rows),
               'rows': [{'check': r[0], 'result': r[1], 'note': r[2]} for r in rows]}, f, indent=2)
for r in rows:
    print(f"{r[1].upper():4}  {r[0]:15} {r[2]}")
print(f"chromium: {passed} of {len(rows)} pass")
sys.exit(0 if passed == len(rows) else 1)
PYEOF
