# Build SeaBIOS

Run `./build/run.sh bash guest/build-seabios.sh` to build only SeaBIOS. The
guest asset build runs it for x86_64. The script pins release 1.17.0, upstream
commit `b52ca86e094d19b58e2304417787e96b940e39c6`, archive SHA256
`5159a35fac5c1579dd5d702001033c826ce45d30d3bd657ec1b2695455997712`,
and epoch 1749578690. `EXTRAVERSION=-vmlab` avoids host/time version strings.
Python bytecode is excluded from the source bundle because it embeds paths.

`guest/dist/firmware/x86_64/bios-vmlab-q35.bin` disables ATA, floppy and flash
floppy drivers. AHCI and virtio-blk remain enabled. The separate
`bios-vmlab-legacy.bin` retains IDE and floppy support for i440fx guests.
Both images are 256 KiB. The expanded configs, `SEABIOS-BUILDINFO`, licence
files and complete corresponding source are shipped beside them. This build
does not compile or embed VGA or NIC ROMs: the virtual VGA uses its external
SeaVGABIOS, and NICs use the separately built iPXE ROM.

QEMU selects the appropriate image for its PC machine type; vmlab-hv selects
the q35 image. Firmware overrides are searched before bundled assets. Without
a bundled SeaBIOS image, QEMU retains its host firmware behavior.

The local patch paints `bootsplash.jpg` after VGA console initialization,
performs useful POST/device work, and restores text mode before the bootloader.
The default boot-menu wait is zero and prints no ESC prompt. QEMU passes a
zero wait for the bundled images. There is no minimum splash duration, so a
fast boot can make it imperceptible. Host/distro fallback firmware retains
the previous one-second splash setting.

vmlab-hv currently cannot supply `bootsplash.jpg` through its machine protocol.
The image still boots without a delay, but branding there needs the orchestrator's
fw_cfg extension; no hypervisor source is changed in this branch.
