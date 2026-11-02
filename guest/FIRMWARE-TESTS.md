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

At the FW5 checkpoint, the pinned vmlab-hv protocol at 41e53e474b537e6d6de48a34ad55c317d8d95ab7
has only firmware kind, CODE, VARS, secure_boot and bootorder in Boot::Firmware.
It needs a default-empty named-byte-file collection, passed by its q35 builder
to FwCfg::add_file before fw_cfg realization. FW5 needs `opt/vmlab/gop-width`
and `opt/vmlab/gop-height` containing decimal ASCII, without a required NUL.
SeaBIOS branding needs `bootsplash.jpg` with JPEG bytes and
`etc/boot-menu-wait` with a little-endian u16 zero. The protocol should reject
duplicate/reserved names and oversized data and test firmware consumption.
That dependency is now resolved by the 450de8c integration recorded below.
No hypervisor files are changed in this repository.

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

## Microsoft 2023 trust-store follow-up

The exact Debian inventory is committed as
`guest/secureboot/debian-inventory.json`: one Debian PK, three KEK certificates
including Microsoft's 2011 and 2023 KEKs, all five requested db certificates,
and 443 SHA256 dbx image hashes. Every requested 2023 CA was already present.
Microsoft's latest published v1.7.1 compatible x64 dbx adds two hashes and
removes none, producing 445 entries without revoking either 2011 CA.

The FW6 Microsoft mode verifies all official certificate fingerprints and
appends compatible revocations to the pinned pristine Debian input. Host and
Ubuntu 24.04 container runs produce byte-identical VARS and
SECUREBOOT-BUILDINFO.json. The original remains selectable as
OVMF_VARS_4M.ms.debian.fd. Both files remain 540,672 bytes. The new default
SHA256 is dd4e1c140f74bbc0ef40caa6b7965be04a198a92733a8808d0080a53f388b32d.
CODE images are unchanged; this follow-up does not rebuild them.

QEMU boots a Windows 11 clone with the new default store and copied TPM state;
Confirm-SecureBootUEFI returns True. Ubuntu also boots with SecureBoot=1.
An additional Linux run sets BootNext to the explicit Ubuntu shimx64.efi entry,
restarts, confirms that entry as BootCurrent, and reaches the guest agent with
SecureBoot=1. Raw PK/KEK/db/dbx bytes are read inside both guest families,
parsed on the host and compared against the expected entries. Both inventories
contain one PK, three KEK certificates, five db certificates and 445 dbx hashes.
All expected 2011 and 2023 certificate fingerprints match.

Evidence is under `.scratch/boot-logo/linux-qemu-ms2023/`,
`windows-qemu-ms2023/` and `linux-qemu-ms2023-shim/`: guest-inventory.json,
guest-variables-base64.json, guest-proof.txt, secure-boot-proof.txt on Linux,
result.json and captures. The source disk modification times and VARS hashes
remain unchanged. Test VMs are stopped and their isolated runtime directories
removed. Installed firmware and owner lab VMs are untouched.

Eleven focused enrollment tests cover deterministic generation and
preservation, refusal of modified stores, corrupt inputs and certificate-level
dbx revocations, plus the existing FW6 cases. Shell syntax, ShellCheck, Python
lint/format and manual validation/rendering pass. No authenticated key update
to an existing VM was attempted: that needs the appropriate PK/KEK-signed
payload and guest servicing policy, not offline replacement of VARS.

## vmlab-hv fw_cfg integration

Both vmlab-hv dependencies now pin
450de8c37bfc1235f9404308de1429e9bb037f5c. The launch description sends GOP
dimensions as inline decimal bytes, SeaBIOS branding as a host-file JPEG, and
its delay as a little-endian u16. Bundled SeaBIOS uses delay zero and no boot
menu; stock fallback uses 1000 ms and enables Boot::Firmware.boot_menu. The
FW5 validation refusal is removed. The binary must match the new protocol;
older strict launch parsers cannot accept the fw_cfg field.

The new pin also adds debugger wire variants. The adapter ignores debugger
stop events for lifecycle purposes, refuses a debugger outcome as a lifecycle
acknowledgement, and the lifecycle fake returns unsupported for debugger
commands. No debugger feature is added. An initial focused compile caught the
three exhaustive-match changes; the corrected run passes 43 adapter tests,
12 hypervisor-configuration tests and 167 QEMU tests, plus the CLI build.

Actual vmlab CLI boots use a copied binary from the clean upstream 450de8c
checkout, SHA256 dfa48b9c3593d40876632be3580bca8020ba92f242a44a597a57c0c9576e8b92.
No installed binary or firmware is replaced. With a saved 800x600 preference,
firmware frames are 1024x768 before bootloader handoff; a following capture
shows the vmlab wordmark at that size. The saved PlatformConfig remains
800x600. The Ubuntu guest reaches agent-ready (`fwcfg-hv-gop`).

SeaBIOS shows the vmlab wordmark during POST and boots the prepared Ubuntu
BIOS fixture to agent-ready (`fwcfg-hv-bios`). Its captured machine.json has
bootsplash.jpg, delay bytes [0,0], and boot_menu false. GOP CODE and settings
also match their captured machine.json. The capture-heavy single runs take
10.218 s and 10.074 s to agent-ready respectively; these are functional checks,
not performance comparisons. The source backing disks and VARS remain
unchanged, and no test VM remains running.

Screenshots:

- `.scratch/firmware-followups/fwcfg-hv-gop/boot-frames/0003.png`, 1024x768 logo.
- `.scratch/firmware-followups/fwcfg-hv-bios/boot-frames/0002.png`, zero-wait POST splash.

The same directories retain launch descriptions, timestamped captures, guest
proofs and results. The 1000 ms stock-fallback branch is covered by adapter
tests; it is not separately booted in this integration check. Snapshot restore
with changed fw_cfg bytes is not tested here; upstream defines those bytes as
read-only snapshot inputs and covers their matching in its S1 tests.
