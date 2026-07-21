#!/usr/bin/env bash
# Pull the virtio-win bits the unattended install needs into unattend/:
#   unattend/drivers/{viostor,netkvm}/   boot-critical virtio drivers (WinPE)
#   unattend/virtio-win-gt-x64.msi       full guest driver/tool package
# These are redistributable binaries and stay out of git (.gitignore).
# (The vmlab guest agent rides in on the auto-attached VMLAB ISO.)
set -euo pipefail
cd "$(dirname "$0")"

VIRTIO_ISO="${VIRTIO_ISO:-/tmp/vmlab-fetch/virtio-win.iso}"
URL="https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso"

if [[ ! -f "$VIRTIO_ISO" ]]; then
    mkdir -p "$(dirname "$VIRTIO_ISO")"
    echo "downloading virtio-win.iso..."
    curl -fSL --retry 3 -o "$VIRTIO_ISO" "$URL"
fi

# bsdtar preserves the ISO's read-only modes, so re-add +w before cleanup.
tmp=$(mktemp -d)
trap 'chmod -R u+w "$tmp" 2>/dev/null; rm -rf "$tmp"' EXIT
bsdtar -xf "$VIRTIO_ISO" -C "$tmp"

# Newest server driver directory the ISO carries (2k25 once present).
osdir() {
    local drv=$1
    for os in 2k25 2k22 2k19; do
        [[ -d "$tmp/$drv/$os/amd64" ]] && { echo "$os"; return; }
    done
    echo "no server driver dir under $drv/" >&2
    exit 1
}

rm -rf unattend/drivers
for drv in viostor NetKVM; do
    os=$(osdir "$drv")
    dest="unattend/drivers/${drv,,}"
    mkdir -p "$dest"
    cp "$tmp/$drv/$os/amd64/"* "$dest/"
    echo "drivers: $drv/$os/amd64 -> $dest"
done

cp "$tmp/virtio-win-gt-x64.msi" unattend/
echo "MSIs staged into unattend/"
