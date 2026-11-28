#!/usr/bin/env bash
# The nightly run of K4 of 3e (card #1187): `just tcg-a64-k4` from the reset
# of the debian-13 template to agent ready, against QEMU, off the merge bar.
# Each run writes run.log, summary.txt and the serial logs into
# <results>/<date>-<commit>/, and appends one line to <results>/history.log.
# The run goes past 16 CPU differences, so that one night shows more than the
# first; summary.txt names the first one.
#
#   scripts/k4-nightly.sh <results directory>
#
# It runs in the hv/ directory of the checkout that holds it. The nightly job
# (`just tcg-a64-k4-nightly-install`, hv/README.md "The K4 nightly run")
# moves that checkout to the newest commit of its branch first, and holds the
# lock that keeps a second run from starting.
set -eu
results=${1:-}
case "$results" in ""|/) echo "k4-nightly: give the results directory" >&2; exit 2;; esac
hv=$(cd "$(dirname "$0")/.." && pwd)
[ -n "$hv" ] && [ "$hv" != / ] || exit 2
mkdir -p -- "$results"
cd "$hv"
commit=$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
run="$results/$(date +%Y%m%d-%H%M%S)-$commit"
mkdir -p -- "$run"
firmware="$hv/../guest/dist/firmware/aarch64"
if [ ! -f "$firmware/AAVMF_CODE.fd" ]; then
	"$hv/../guest/fetch-firmware.sh" aarch64
fi
started=$(date +%s)
status=0
just tcg-a64-k4 "$run" '' "$firmware" 2048 0 16 > "$run/just.log" 2>&1 || status=$?
ended=$(grep -m 1 '^ended:' "$run/run.log" 2>/dev/null || echo "ended: no report (see just.log)")
echo "$(date -Is) commit=$commit exit=$status hours=$(( ($(date +%s) - started) / 3600 )) $ended" >> "$results/history.log"
exit "$status"
