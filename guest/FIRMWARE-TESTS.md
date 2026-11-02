# Firmware validation

## FW1: selective UEFI connection

Measurements on 2026-11-02 used the same stopped Ubuntu 24.04.4 clone as a
read-only qcow2 backing file, a fresh overlay and the same copied existing VARS
for each boot. Both builds are RELEASE with edk2 networking enabled. Each VM
has two vCPUs, 4 GiB RAM and one NIC. Timing starts when the host sends CPU
resume. UEFI handoff is the first `BdsDxe: starting Boot` serial message
(10 ms polling); agent-ready is daemon status (50 ms polling). Three samples
per cell are summarized by the median. This is a shared host, not an isolated
performance benchmark.

| Hypervisor | UEFI handoff before | UEFI handoff FW1 | Agent-ready before | Agent-ready FW1 |
| --- | ---: | ---: | ---: | ---: |
| QEMU | 1.599 s | 1.731 s | 7.246 s | 7.256 s |
| vmlab-hv | 0.773 s | 0.628 s | 5.824 s | 5.285 s |

Selective connection improves this vmlab-hv sample but regresses QEMU's firmware
handoff by 133 ms. There is no demonstrated QEMU end-to-end improvement from
FW1 alone. The timeout was already zero before the change; the patch prevents
host menu settings from adding a delay. It does not remove an existing delay
from the default configuration.

Focused checks: the two CODE builds complete, their generated blank VARS match
Debian byte for byte, the QEMU firmware/cmdline tests pass, and the vmlab-hv
description tests pass. All six candidate Ubuntu boots reach agent-ready.
Stock/custom OVMF without the feature manifest receives no connection hints.

Raw runs are under `.scratch/firmware-followups/`: `baseline-qemu-2` through
`baseline-qemu-4`, `baseline-hv-fixed3`, `baseline-hv-4`, `baseline-hv-5`,
`fw1-qemu-1` through `fw1-qemu-3`, and `fw1-hv-1` through `fw1-hv-3`.
Each successful run retains its result, event timestamps, guest proof and
screenshot. Early harness failures are not included. The early QEMU baseline
runs required explicit cleanup after force-stop; that harness defect was fixed
before the candidate runs. Source disk modification time and VARS hash remained
unchanged. Test VMs were limited to six host-wide and scoped to 24 GiB with
swap disabled.

## FW2: network-free OVMF and iPXE

All four RELEASE CODE variants build with unchanged flash sizes and a generated
blank VARS identical to Debian's. A clean iPXE rebuild produces identical hashes
for all four combined ROMs, both individual virtio ROMs and the corresponding
source archive. The build uses GCC 13.3.0 and binutils 2.42 in the build container.

QEMU boots both SeaBIOS and network-free OVMF through the built virtio iPXE ROM,
obtains a DHCP boot URL, fetches an HTTP script and executes its unique success
marker. Runs: `ipxe-bios-2` and `ipxe-uefi-minimal2`. EFI console and iPXE serial
output duplicate characters on COM1; the assertion recognizes that actual output.
The first UEFI assertion timed out despite the HTTP fetch and marker; it is not
counted as a passing test.

Ubuntu reaches agent-ready using network-free CODE on QEMU and vmlab-hv, and
using the native-network variant selected through `firmware_network_stack` on
QEMU (`fw2-qemu`, `fw2-hv`, `fw2-native`). QEMU and configuration focused tests
pass. `wcl check src/config/schema.wcl` and `wcl check docs/manual/main.wcl` pass.

## FW3: reproducible SeaBIOS

The existing Ubuntu image is UEFI-only: stock SeaBIOS stops at `Booting from
Hard Disk`. A disposable overlay installs `grub-pc-bin`, reuses its former ESP
as a BIOS boot partition, and masks that ESP's mount. The source disk is never
changed. All BIOS timing runs use fresh overlays of this same prepared disk.
The baseline uses the logo PR's host SeaBIOS and one-second splash; the candidate
uses the new bundled image, zero wait and the built iPXE ROM. OS configuration
is identical between the two groups. CPU-resume to daemon agent-ready has a
median of **6.836 s before, 5.137 s after** over three boots per group. Ranges
are 6.457–7.765 s before and 4.937–5.275 s after on this shared host.

Runs: `baseline-bios-1` through `baseline-bios-3` and `fw3-bios-1` through
`fw3-bios-3`. `fw3-bios-hv` also reaches agent-ready on vmlab-hv. The POST
capture `bios-post-capture/0099.png` shows the vmlab splash with a zero host
menu wait. No minimum splash duration is added.

Two clean builds produce identical BIOS images, expanded configs, source
archive and BUILDINFO. The first source-archive comparison failed because
Python bytecode embedded temporary build paths; disabling bytecode and
excluding its cache resolves the same comparison. Focused QEMU and vmlab-hv
tests pass. The manual validates. The legacy image builds with IDE/floppy
drivers, but a legacy guest boot was not exercised here.

## FW4: opt-in DEBUG OVMF

All eight CODE combinations build and retain the Debian VARS layout/content.
Ubuntu reaches agent-ready with `firmware_debug = true` on QEMU and vmlab-hv,
and with both debug and native-network selectors on QEMU (`fw4-qemu`, `fw4-hv`,
`fw4-native`). Their machine `serial.log` files contain SEC/PEI/DXE diagnostics
beginning with `SecCoreStartupWithStack`, not only the normal UART banner.
QEMU's debug port and UART share an append-only multiplexed file. The existing
pinned vmlab-hv capture needs no source change. DEBUG firmware timing is not
compared with RELEASE timing. Focused QEMU/configuration tests and manual/schema
validation pass.

## FW5: host-selected OVMF resolution

All eight CODE variants build with the resolution patch and retain the Debian
VARS layout. A GOP probe with a saved 800x600 PlatformConfig selects 1024x768
when both decimal fw_cfg hints are valid. Malformed, missing, out-of-range and
absent hints each retain 800x600. All five runs reparse the resulting VARS and
confirm the saved PlatformConfig bytes remain unchanged. The probe CODE hash
matches the shipped RELEASE image; no bootable disk is attached for these
captures.

The shipped RELEASE image also boots Ubuntu to agent-ready through the actual
host fields on QEMU (`fw5-host-size`, 7.220 s). With no hints it boots on
vmlab-hv (`fw5-hv-default`, 7.973 s). These single runs demonstrate bootability,
not a speed comparison. CLI validation accepts a valid pair and rejects a half
pair, excessive dimensions and explicit vmlab-hv resolution settings. Starting
with an override lacking the capability manifest fails before VM launch
(`fw5-unsupported`).

The continuation review repeats all five GOP cases against the shipped image
(`review2-gop-*`) and boots through the rebuilt CLI's paired fields
(`review-host-size`, 1.457 s handoff, 7.244 s agent-ready). All pass. The
focused checks pass 9 firmware-named Rust tests and 167 QEMU tests; CLI
validation passes all five cases. Shell syntax, isolated ShellCheck, schema
validation and HTML manual rendering also pass.

The pinned vmlab-hv protocol at 41e53e474b537e6d6de48a34ad55c317d8d95ab7
has only firmware kind, CODE, VARS, secure_boot and bootorder in Boot::Firmware.
It needs a default-empty named-byte-file collection, passed by its q35 builder
to FwCfg::add_file before fw_cfg realization. FW5 needs `opt/vmlab/gop-width`
and `opt/vmlab/gop-height` containing decimal ASCII, without a required NUL.
SeaBIOS branding needs `bootsplash.jpg` with JPEG bytes and
`etc/boot-menu-wait` with a little-endian u16 zero. The protocol should reject
duplicate/reserved names and oversized data and test firmware consumption.
After that upstream change, vmlab must update its protocol pin, fill the files
in describe.rs and remove the FW5 refusal. No hypervisor files are changed here.

## FW6: optional Microsoft-plus-lab trust

The offline enrollment tool appends public certificates to db in a new copy
of Microsoft's 540,672-byte VARS. Seven focused tests cover deterministic
output, unchanged source bytes, PEM/DER equivalence, explicit owner GUID,
no overwrite, private-key and blank-template rejection, malformed second
certificate atomicity and empty-input rejection. Python lint and format checks
pass. The script's adjacent uv lock pins the dependency set.

The review runs the actual locked CLI using an ephemeral public test certificate,
then boots a signed EFI probe under the resulting store with SecureBoot=1
(`review2-fw6-signed`). The unsigned probe is rejected under that same store
(`review2-fw6-unsigned`), and the signed probe is rejected under Microsoft-only
VARS (`review2-fw6-ms-only`). All three assertions pass on the final RELEASE
secboot image. The earlier DEBUG/secboot signed-payload check also passes
(`fw6-final-debug-signed`).

The existing Microsoft-signed Ubuntu installation boots under the combined
store through `secure_boot_vars` and reports SecureBoot=1
(`fw6-ubuntu-combined`, 7.957 s agent-ready). The final Windows 11 clone also
boots the current secboot CODE with its existing Microsoft VARS and TPM state;
Confirm-SecureBootUEFI returns True. Its source disk modification time and
VARS hash are unchanged (`.scratch/boot-logo/windows-qemu-firmware-final/`).
These are compatibility checks, not boot-speed measurements.

No production vmlab certificate was supplied. The shipped default remains
Microsoft-only; no signing private key or invented production identity is
included. Lab trust is opt-in via public certificates and the existing custom
VARS path, which both hypervisor adapters already support. No FW6 hypervisor
change is required. OVMF's existing option-ROM policy permits unsigned PCI ROMs;
payload signature enforcement is demonstrated separately above.
