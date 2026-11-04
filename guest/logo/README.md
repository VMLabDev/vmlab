# vmlab logos

`wordmark.svg` is the editable source. It contains paths, so rendering does not
depend on an installed font. The light, bold sans-serif wordmark has a transparent
background. The firmware and SeaBIOS renders use black backgrounds.

Run `guest/logo/render.sh` with `rsvg-convert` and ImageMagick installed after
changing the SVG. Commit both generated assets with the source:

- `Logo.bmp` is a 193 by 58 pixel, uncompressed, 24-bit Windows BMP. It matches
  the dimensions of edk2's `MdeModulePkg/Logo/Logo.bmp`, which is originally 8-bit.
  `guest/build-ovmf.sh` replaces that file before building LogoDxe.
- `bootsplash.jpg` is a 640 by 480 pixel baseline JPEG. The CLI embeds it and
  stages it beside each QEMU SeaBIOS VM's disk at start. QEMU passes it to
  SeaBIOS through `-boot menu=on,splash=...,splash-time=1000`. SeaBIOS displays
  its splash only while the boot menu is enabled.

OVMF publishes its logo through BGRT. Windows can reuse it during boot.
The pinned vmlab-hv protocol has no extra fw_cfg file field, so its SeaBIOS
guests do not receive the splash. Its OVMF guests use the branded CODE normally.

## App icon

`icon.svg` is the editable, path-only app icon: a light, bold `v` on a dark
rounded tile. It uses the wordmark's colour and letter shape, with wider strokes
for small sizes. The transparent outer margin separates it from light and dark
desktop panels.

Run `guest/logo/render-icon.sh` after changing the SVG. Commit the source and
all seven generated PNGs (16, 24, 32, 48, 64, 128, and 256 pixels).
To refresh the hypervisor viewer's embedded 64-pixel RGBA copy as well, pass its
output path:

```sh
guest/logo/render-icon.sh /path/to/vmlab-hypervisor/crates/vmlab-hv-viewer/assets/icon.rgba
```

The hypervisor bundle ships these icons under the hicolor name
`dev.vmlab.viewer`. Its `install-viewer-icons.sh` installs them and the hidden
desktop entry for Wayland app-ID matching.
