#!/bin/sh
# Install only the viewer's desktop entry and hicolor icons for the current user.
set -eu
root=$(dirname "$(readlink -f "$0")")
data=${XDG_DATA_HOME:-"$HOME/.local/share"}
app=dev.vmlab.viewer

# Check the complete payload before changing the user's installation.
test -f "$root/share/applications/$app.desktop"
test -f "$root/share/icons/hicolor/scalable/apps/$app.svg"
for size in 16 24 32 48 64 128 256; do
  test -f "$root/share/icons/hicolor/${size}x${size}/apps/$app.png"
done

install -Dm644 "$root/share/applications/$app.desktop" "$data/applications/$app.desktop"
install -Dm644 "$root/share/icons/hicolor/scalable/apps/$app.svg" \
  "$data/icons/hicolor/scalable/apps/$app.svg"
for size in 16 24 32 48 64 128 256; do
  install -Dm644 "$root/share/icons/hicolor/${size}x${size}/apps/$app.png" \
    "$data/icons/hicolor/${size}x${size}/apps/$app.png"
done
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache --force --ignore-theme-index "$data/icons/hicolor"
fi
printf 'Installed %s desktop entry and icons in %s\n' "$app" "$data"
