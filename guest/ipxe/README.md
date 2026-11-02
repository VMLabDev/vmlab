# Reproducible iPXE ROMs

Run `./build/run.sh python3 guest/ipxe/build.py` to build the PCI ROMs.
The guest asset build runs it after OVMF. The build requires Git, GCC with
32-bit freestanding support, binutils, make, Perl, Python and liblzma headers.
It verifies the source checkout and refuses tracked changes or untracked files.

The source pin and build procedure come from
`vmlab-hypervisor/firmware/ipxe`: iPXE 2.0.0, commit
`12798ec29aa8a64d8675c4378b99f5fe28447afb`, fixed epoch 1741276800,
`BUILD_SERIAL=0`, `NO_WERROR=1`. The local EFI configuration deliberately
undefines `EFI_DOWNGRADE_UX`: a network-free OVMF needs iPXE's LoadFile boot
option, rather than only its SNP interface. Both ROM families use COM1 output
and a zero banner timeout. HTTP and TFTP are common to BIOS and EFI; their full
protocol feature sets are not identical.

Outputs under `guest/dist/firmware/x86_64` include combined legacy/EFI ROMs for
virtio-net, e1000, rtl8139 and pcnet, individual virtio ROMs, and
`IPXE-SHA256SUMS`. QEMU and vmlab-hv prefer these assets over host ROMs.
QEMU's firmware builds are not inputs. `ipxe-source.tar` contains the complete
pinned source, licence notices, configuration and build helper; distribute it
alongside the ROMs to satisfy their applicable GPL source requirements.

The default bundled OVMF disables edk2 networking. A VM with
`firmware_network_stack = true` selects the `.network.fd` CODE variant, which
keeps edk2 IPv4/IPv6 PXE, HTTP and TLS support (including plain HTTP). This is
an x86_64 OVMF setting, declared on the VM only. The VARS template and existing
per-VM store do not change. An override needs the requested variant or start
fails with its missing filename.

The PCI ROMs are unsigned. The pinned OVMF explicitly permits PCI option ROM
execution under secure boot; firmware policy still checks EFI boot payloads.
Changing that policy would require signing the EFI ROM or choosing the native
network stack. Do not describe these ROMs as Microsoft-signed.
