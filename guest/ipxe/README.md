# Reproducible iPXE ROMs

The one iPXE tree. `build.py` builds the pinned upstream iPXE as PCI ROMs in
two profiles, which share everything but their EFI configuration and their
outputs:

- `guest` (the default): the firmware vmlab ships. Run
  `./build/run.sh python3 guest/ipxe/build.py`; the guest asset build runs it
  after OVMF.
- `hv`: the ROM vmlab-hv's own NIC tests boot. Run `just ci::hv-ipxe-build`
  from the repository root, or `just firmware-ipxe-build` in `hv/`.

The build requires Git, GCC with 32-bit freestanding support, binutils, make,
Perl, Python and liblzma headers. The first build of a profile needs network
access to GitHub. It verifies its source checkout and refuses a different
commit, tracked changes or untracked files.

`upstream.json` pins iPXE 2.0.0, commit
`12798ec29aa8a64d8675c4378b99f5fe28447afb`, fixed epoch 1741276800, built with
`BUILD_SERIAL=0` and `NO_WERROR=1`. `config/*.h` is shared: COM1 output beside
the firmware console, and q35's fixed COM1 UART. `config/<profile>/*.h` is
copied over it.

## Why two profiles

They disagree on one setting, upstream `EFI_DOWNGRADE_UX`, and each is right
for the firmware it runs under:

- `guest` undefines it. The OVMF vmlab bundles disables edk2 networking, so
  that OVMF needs iPXE's LoadFile boot option, not only its SNP interface.
- `hv` defines it. OVMF then registers its native IPv4 and IPv6 boot options
  through SNP, without an additional iPXE LoadFile boot option. A black-box
  QEMU comparison with the same OVMF configuration verifies identical
  `Boot####` and `BootOrder` variables:

  ```sh
  HWREF_RECAPTURE=1 python -m unittest discover -s scripts/hwref -p test_p2.py -k own_ipxe
  ```

Both use COM1 output and a zero banner timeout. HTTP and TFTP are common to
BIOS and EFI; their full protocol feature sets are not identical.

## Outputs

`guest` writes to `guest/dist/firmware/x86_64`: combined legacy/EFI ROMs for
virtio-net, e1000, rtl8139 and pcnet, the individual virtio ROMs
(`1af41000.rom`, `1af41000.efirom`), `ipxe-source.tar` and `IPXE-SHA256SUMS`.
QEMU and vmlab-hv prefer these assets over host ROMs; QEMU's firmware builds
are not inputs. Its checkout is `guest/.cache/ipxe/upstream`.

`hv` writes to `hv/target/ipxe`: `1af41000.rom` (legacy BIOS ROM for
`1af4:1000`), `1af41000.efirom` (x86_64 EFI ROM), `virtio-net.rom` (both,
combined), `ipxe-source.tar` and `SHA256SUMS`. `SHA256SUMS.hv` records them.
Its checkout is `hv/target/ipxe/upstream`.

The combined images use upstream `util/catrom.pl`, which updates the PCI
last-image flag and checksum; the virtio-net one fits the captured 256 KiB NIC
ROM BAR. A clean rebuild with GCC 16.2.1 and binutils 2.47 produced identical
ROM hashes; other toolchain versions can produce different bytes.

## Use and verify (hv)

Set a q35 NIC's optional `rom` field to the absolute path of `1af41000.rom`
for SeaBIOS, or `virtio-net.rom` for a combined image. Omitting `rom`
disables the expansion ROM. The factory reads the file and calls
`VirtioPciDevice::new_transitional(...).with_option_rom(&image)` before PCI
registration.

Run `cargo test -p vmlab-hv-devices transitional_tests -- --nocapture` to
check the transport and built ROMs. Run
`cargo test -p vmlab-hv --test ipxe -- --nocapture` for the P1 control-socket
probe, `boot.py`. It requires KVM, SeaBIOS, SeaVGABIOS, and a built ROM;
missing assets produce a named skip. The probe resumes a paused q35 machine
and checks for iPXE on serial or in VGA text memory, read by the VMM's
diagnostic output when the probe quits through the control socket. The legacy
ROM prefix uses BIOS text services before initializing iPXE's serial console.

The hwref collector records NIC ROMs as `guest/pci-roms/<BDF>.rom`, with each
`:` in the BDF written as `-` (`0000-00-03.0.rom`) so Windows can check it out. The diff
compares PCI ROM vendor/device identity and image types, while lspci retains
ROM presence and BAR size. Executable bytes are not compared.

## OVMF networking

The default bundled OVMF disables edk2 networking. A VM with
`firmware_network_stack = true` selects the `.network.fd` CODE variant, which
keeps edk2 IPv4/IPv6 PXE, HTTP and TLS support (including plain HTTP). This is
an x86_64 OVMF setting, declared on the VM only. The VARS template and existing
per-VM store do not change. An override needs the requested variant or start
fails with its missing filename.

## Signing, licence and corresponding source

The PCI ROMs are unsigned. The pinned OVMF explicitly permits PCI option ROM
execution under secure boot; firmware policy still checks EFI boot payloads.
Changing that policy would require signing the EFI ROM or choosing the native
network stack. Do not describe these ROMs as Microsoft-signed.

iPXE is separate firmware, not MIT-licensed hypervisor code. Its source
carries GPLv2-or-later and other licence notices. Each profile's
`ipxe-source.tar` holds the complete pinned source, those notices, this build
script and the profile's configuration: distribute it alongside the ROMs to
meet the applicable GPLv2 terms, rather than relying on an upstream download
link as the source offer.

Upstream build documentation: https://ipxe.org/download

Upstream source: https://github.com/ipxe/ipxe
