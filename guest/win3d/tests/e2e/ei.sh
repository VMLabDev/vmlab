#!/usr/bin/env bash
# The check for patches/vkd3d-proton/0007 (docs/ei.md): ExecuteIndirect with
# command signatures that set vertex and index buffer views draws what WARP
# draws. Runs in a lab where the package is installed and the autologon
# desktop is up (a lab kept with tests/e2e/run.sh --keep).
#
#   tests/e2e/ei.sh <lab-dir> <vm> <evidence-dir>
#
# It builds probes/d3d12-ei when out/probes-extra/ has no ei_views_probe.exe
# and runs the x64 and the x86 probe in the user's session with
# tests/guest/Run-Probe.ps1. The probe draws its scene on the default adapter
# and on WARP, and compares both with the expected cells and with each other.
# A run passes when the probe passed, the default adapter is not a software
# renderer, and vkd3d-proton logged that it emulates all five of the probe's
# command signatures by fetching vertices: without those lines the path under
# test did not run. Writes <evidence-dir>/ei/: ei.txt (the verdict),
# ei-<arch>.json (the probe), ei-<arch>.gpu.bmp and ei-<arch>.warp.bmp (the
# two pictures), ei-<arch>.vkd3d.log and run-<arch>.json (Run-Probe.ps1).
#
# Exit status: 0 when both pass, 1 when one fails, 2 on usage.
# check-shell-paths: no-errexit each probe run records its result and the check judges both (VML-88).
set -uo pipefail
[ $# -eq 3 ] || { sed -n '2,20p' "$0" >&2; exit 2; }
REPO=$(cd "$(dirname "$0")/../.." && pwd)
LABDIR=$(cd "$1" && pwd)
VM=$2
mkdir -p "$3/ei"
EV=$(cd "$3/ei" && pwd)   # absolute: the vmlab commands run from the lab directory
OUT=$REPO/out/probes-extra
R='C:\win3d\results\ei'

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { in_lab vmlab exec --timeout "${2:-600}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'; }

[ -n "$EV" ] && [ -d "$EV" ] && [ "$EV" != / ] || { echo "VML-88: the evidence directory is empty or missing" >&2; exit 2; }
find "$EV" -mindepth 1 -delete
if [ ! -f "$OUT/x64/ei_views_probe.exe" ] || [ ! -f "$OUT/x86/ei_views_probe.exe" ]; then
    note "building probes/d3d12-ei"
    "$REPO/probes/d3d12-ei/build.sh" >"$EV/build.log" 2>&1 || { tail -5 "$EV/build.log"; exit 1; }
fi
in_lab vmlab cp "$REPO/tests/guest" "$VM:C:/win3d/guest" >/dev/null || exit 1
gps "New-Item -ItemType Directory -Force '$R' | Out-Null; Remove-Item '$R\\*' -ErrorAction SilentlyContinue" 60

failed=0
for arch in x64 x86; do
    note "ei_views_probe $arch"
    in_lab vmlab cp "$OUT/$arch/ei_views_probe.exe" "$VM:C:/win3d/probes-extra/$arch/ei_views_probe.exe" >/dev/null || exit 1
    gps "& 'C:\\win3d\\guest\\Run-Probe.ps1' -Path 'C:\\win3d\\probes-extra\\$arch\\ei_views_probe.exe' -NoModules -Name ei-$arch -OutDir '$R' -Seconds 60" 500 >"$EV/run-$arch.json" 2>&1
    for f in json gpu.bmp warp.bmp vkd3d.log; do
        in_lab vmlab cp "$VM:C:/win3d/results/ei/ei-$arch.$f" "$EV/ei-$arch.$f" >/dev/null 2>&1
    done
    python3 - "$arch" "$EV/ei-$arch.json" <<'PY' | tee -a "$EV/ei.txt"
import json, re, sys
arch, result = sys.argv[1:3]
SOFTWARE = re.compile(r'basic render|warp|llvmpipe|lavapipe|swiftshader|software', re.I)
try:
    d = json.load(open(result))
except (OSError, ValueError):
    print(f'fail   {arch}: the probe wrote no result')
    sys.exit(0)
what = (f"{d.get('wrong_cells')} of {d.get('cells')} cells wrong on {d.get('adapter')!r}, "
        f"{d.get('wrong_cells_warp')} on WARP, "
        f"{d.get('pixels_differ_from_warp')} of {d.get('pixels')} pixels differ from WARP, "
        f"{d.get('emulated_signatures')} of 5 signatures emulated, "
        f"{d.get('dropped_draw_reports')} reports of dropped draws")
if d.get('result') == 'error':
    print(f"fail   {arch}: the probe could not draw: {'; '.join(map(str, d.get('errors', [])))}")
elif d.get('result') != 'pass':
    print(f'fail   {arch}: {what}')
elif SOFTWARE.search(d.get('adapter') or 'software'):
    print(f'fail   {arch}: the default adapter is a software renderer ({what})')
elif d.get('emulated_signatures') != 5:
    print(f'fail   {arch}: vkd3d-proton did not log five emulated signatures, so the path was not reached ({what})')
else:
    print(f'pass   {arch}: {what}')
PY
done
grep -q '^fail' "$EV/ei.txt" && failed=1
[ "$(grep -c '^pass' "$EV/ei.txt")" = 2 ] || failed=1
exit $failed
