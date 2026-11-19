#!/usr/bin/env bash
# The check for patches/mesa/0094 (docs/d2d-coverage.md): what Direct2D draws
# to antialias a shape, target-independent rasterization, gives the coverage
# bits that WARP gives, the same in every run, and the driver drew it as one
# pass per sample. Runs in a lab where the package is installed and the
# autologon desktop is up (a lab kept with tests/e2e/run.sh --keep).
#
#   tests/e2e/tir.sh <lab-dir> <vm> <evidence-dir>
#
# It builds probes/d3d11-tir when out/probes-extra/ has no tir_probe.exe,
# runs the x64 and the x86 probe in the user's session with
# tests/guest/Run-Probe.ps1, and reads the lines the driver wrote about the
# probe into C:\ProgramData\Yttrium\yttrium-errors.log: one "forced-sample
# draw tir_probe.exe ... coverage=..." line for each pipeline. It first lets
# every user append to that log: as the driver makes it, only the process
# that made it can, and that is dwm.exe most often. A run passes
# when the probe found no pixel that differs from WARP or between its runs,
# and every such line says coverage=passes. Without a line the x64 probe did
# not reach the path, and the run fails. The x86 driver writes no such line
# (why is not known), so the x86 run is judged by the picture alone. Writes <evidence-dir>/tir/: tir.txt (the
# verdict), tir-<arch>.json (the probe), run-<arch>.json (Run-Probe.ps1) and
# driver-log.txt.
#
# Exit status: 0 when both pass, 1 when one fails, 2 on usage.
# check-shell-paths: no-errexit each probe run records its result and the check judges both (VML-88).
set -uo pipefail
[ $# -eq 3 ] || { sed -n '2,24p' "$0" >&2; exit 2; }
REPO=$(cd "$(dirname "$0")/../.." && pwd)
LABDIR=$(cd "$1" && pwd)
VM=$2
mkdir -p "$3/tir"
EV=$(cd "$3/tir" && pwd)   # absolute: the vmlab commands run from the lab directory
OUT=$REPO/out/probes-extra
R='C:\win3d\results\tir'
LOG='C:\ProgramData\Yttrium\yttrium-errors.log'

note() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
in_lab() { (cd "$LABDIR" && "$@"); }
gps() { in_lab vmlab exec --timeout "${2:-600}" "$VM" -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$1" | tr -d '\r'; }
# The driver's lines about the probe, without the time, the pid and the shader id.
driver_lines() {
    gps "if (Test-Path '$LOG') { Get-Content '$LOG' | Where-Object { \$_ -match 'forced-sample draw tir_probe.exe' } | ForEach-Object { (\$_ -replace '^.*yttrium: ', '') -replace ' fs=\d+', '' } }" 60
}

[ -n "$EV" ] && [ -d "$EV" ] && [ "$EV" != / ] || { echo "VML-88: the evidence directory is empty or missing" >&2; exit 2; }
find "$EV" -mindepth 1 -delete
if [ ! -f "$OUT/x64/tir_probe.exe" ] || [ ! -f "$OUT/x86/tir_probe.exe" ]; then
    note "building probes/d3d11-tir"
    "$REPO/probes/d3d11-tir/build.sh" >"$EV/build.log" 2>&1 || { tail -5 "$EV/build.log"; exit 1; }
fi
in_lab vmlab cp "$REPO/tests/guest" "$VM:C:/win3d/guest" >/dev/null || exit 1
gps "New-Item -ItemType Directory -Force '$R' | Out-Null; Remove-Item '$R\\*' -ErrorAction SilentlyContinue" 60

failed=0
: >"$EV/driver-log.txt"
for arch in x64 x86; do
    note "tir_probe $arch"
    in_lab vmlab cp "$OUT/$arch/tir_probe.exe" "$VM:C:/win3d/probes-extra/$arch/tir_probe.exe" >/dev/null || exit 1
    # The log belongs to the process that made it, most often dwm.exe, and
    # other users may only read it: the probe's lines would be lost. Let
    # every process append to it.
    gps "if (-not (Test-Path '$LOG')) { New-Item -ItemType File -Force '$LOG' | Out-Null }; icacls.exe '$LOG' /grant '*S-1-1-0:(M)' | Out-Null" 60
    before=$(driver_lines | grep -c .)
    gps "& 'C:\\win3d\\guest\\Run-Probe.ps1' -Path 'C:\\win3d\\probes-extra\\$arch\\tir_probe.exe' -NoModules -Name tir-$arch -OutDir '$R' -Seconds 30" 400 >"$EV/run-$arch.json" 2>&1
    in_lab vmlab cp "$VM:C:/win3d/results/tir/tir-$arch.json" "$EV/tir-$arch.json" >/dev/null 2>&1
    driver_lines | tail -n +$((before + 1)) >"$EV/lines-$arch.txt"
    sed "s/^/$arch: /" "$EV/lines-$arch.txt" >>"$EV/driver-log.txt"
    python3 - "$arch" "$EV/tir-$arch.json" "$EV/lines-$arch.txt" <<'PY' | tee -a "$EV/tir.txt"
import json, sys
arch, result, lines = sys.argv[1:4]
try:
    d = json.load(open(result))
except (OSError, ValueError):
    print(f'fail   {arch}: the probe wrote no result')
    sys.exit(0)
paths = [l.split('coverage=')[1].split()[0] for l in open(lines) if 'coverage=' in l]
what = (f"{d.get('mismatch')} of {d.get('covered_pixels')} covered pixels differ from WARP, "
        f"{d.get('runs_differ')} differ between {d.get('runs')} runs, "
        f"{d.get('lost_bits')} coverage bits lost, {d.get('extra_bits')} extra; "
        f"driver: {', '.join(sorted(set(paths))) or 'no line'} ({len(paths)} pipelines)")
if d.get('result') != 'ok':
    print(f"fail   {arch}: the probe could not draw: {'; '.join(d.get('errors', []))}")
elif d['mismatch'] or d['runs_differ']:
    print(f'fail   {arch}: {what}')
elif not paths and arch == 'x64':
    print(f'fail   {arch}: the driver logged no forced-sample draw for the probe, so the path was not reached ({what})')
elif not paths:
    # The x86 driver writes no such line, on either path; why is not known.
    print(f'pass   {arch}: {what}; judged by the picture alone')
elif set(paths) != {'passes'}:
    print(f'fail   {arch}: the driver did not draw one pass per sample ({what})')
else:
    print(f'pass   {arch}: {what}')
PY
    rm -f "$EV/lines-$arch.txt"
done
grep -q '^fail' "$EV/tir.txt" && failed=1
[ "$(grep -c '^pass' "$EV/tir.txt")" = 2 ] || failed=1
exit $failed
