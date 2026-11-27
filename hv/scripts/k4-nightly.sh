#!/usr/bin/env bash
# The nightly run of K4 of 3e (card #1187): `just tcg-a64-k4` from the reset
# of the debian-13 template to agent ready, against QEMU, off the merge bar.
# Each run writes run.log, summary.txt and the serial logs into
# <results>/<date>-<commit>/, and appends one line to <results>/history.log.
# A run that finds the one before it still running ends at once.
#
#   scripts/k4-nightly.sh <results directory>
#
# It runs in the hv/ directory of the checkout that holds it. On helios the
# job is a user timer of the vmlab account that runs this script through
# `stc run`, which first moves the checkout to the newest pushed commit of
# the branch (hv/README.md, "K4 nightly").
set -eu
results=${1:-}
case "$results" in ""|/) echo "k4-nightly: give the results directory" >&2; exit 2;; esac
hv=$(cd "$(dirname "$0")/.." && pwd)
[ -n "$hv" ] && [ "$hv" != / ] || exit 2
mkdir -p -- "$results"
exec 9> "$results/.lock"
if ! flock -n 9; then
	echo "k4-nightly: a run is still going on" >&2
	exit 0
fi
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
just tcg-a64-k4 "$run" '' "$firmware" > "$run/just.log" 2>&1 || status=$?
ended=$(grep -m 1 '^ended:' "$run/run.log" 2>/dev/null || echo "ended: no report (see just.log)")
echo "$(date -Is) commit=$commit exit=$status hours=$(( ($(date +%s) - started) / 3600 )) $ended" >> "$results/history.log"
exit "$status"
