#!/usr/bin/env bash
# Copy what labs/zeus/run.sh needs from helios to Zeus's staging tree
# (~/win3d-zeus/repo), laid out like this repo, and fetch its evidence back.
#
#   labs/zeus/stage.sh [E2E_REF]      push: out/package, out/probes, tests/guest,
#                                     scripts/install-into-vm.sh, installer/VERSION,
#                                     labs/zeus and
#                                     tests/e2e as of git ref E2E_REF (default main)
#   labs/zeus/stage.sh --fetch GPU    pull docs/evidence/zeus/GPU back
#
# Build the package first with scripts/build.sh. Set ZEUS to another ssh host.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../.." && pwd)
ZEUS=${ZEUS:-zeus}
DEST=win3d-zeus/repo
if [ "${1:-}" = --fetch ]; then
    mkdir -p "$REPO/docs/evidence/zeus/$2"
    [ -n "$REPO" ] && [ "$REPO" != / ] || { echo "VML-88: REPO is empty or /" >&2; exit 1; }
    rsync -a --delete "$ZEUS:$DEST/docs/evidence/zeus/$2/" "$REPO/docs/evidence/zeus/$2/"
    exit 0
fi
E2E=${1:-main}
ssh "$ZEUS" "mkdir -p $DEST/out $DEST/tests $DEST/scripts $DEST/installer $DEST/labs $DEST/patches"
rsync -a --delete "$REPO/out/package/" "$ZEUS:$DEST/out/package/"
rsync -a --delete "$REPO/out/probes/" "$ZEUS:$DEST/out/probes/"
rsync -a --delete "$REPO/tests/guest/" "$ZEUS:$DEST/tests/guest/"
ssh "$ZEUS" "rm -rf $DEST/tests/e2e"
git -C "$REPO/../.." archive "$E2E:guest/win3d" tests/e2e | ssh "$ZEUS" "tar -C $DEST -xf -"
rsync -a "$REPO/scripts/install-into-vm.sh" "$ZEUS:$DEST/scripts/"
rsync -a "$REPO/installer/VERSION" "$ZEUS:$DEST/installer/"
rsync -a --delete "$REPO/labs/zeus/" "$ZEUS:$DEST/labs/zeus/"
# labs/zeus/uavtest/build.sh output, when built.
[ ! -d "$REPO/out/labs-zeus" ] || rsync -a --delete "$REPO/out/labs-zeus/" "$ZEUS:$DEST/out/labs-zeus/"
{
    echo "repo $(git -C "$REPO" rev-parse --short HEAD)$(git -C "$REPO" diff --quiet HEAD -- labs/zeus || echo -dirty)"
    echo "tests/e2e from $E2E $(git -C "$REPO" rev-parse --short "$E2E")"
    echo "package $(python3 -c 'import json,glob; print(json.load(open(glob.glob("'"$REPO"'/out/package/*/manifest.json")[0]))["commit"])')"
} | ssh "$ZEUS" "cat > $DEST/STAGED"
ssh "$ZEUS" "cat $DEST/STAGED"
