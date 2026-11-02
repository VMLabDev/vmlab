# Build branded OVMF

The guest build runs `fetch-firmware.sh`, then `build-ovmf.sh` for x86_64.
Both `just guest-install` and `just guest-package` use this path.
To build only the firmware in the release toolchain container, run:

```sh
just buildbox::image
./build/run.sh bash -c './guest/fetch-firmware.sh x86_64 && ./guest/build-ovmf.sh'
```

The output is `guest/dist/firmware/x86_64/`. `BUILDINFO` records source pins,
compiler versions, the logo digest, and SHA256 hashes of CODE and VARS.
Debian's package version remains in `VERSION`. `COPYRIGHT` carries edk2 and
dependency notices from the pinned package. The build never installs firmware
or writes a VM's variable store.

## Source and configuration

Debian's `edk2` source package `2026.08+ds-2` repacks upstream
`edk2-stable202608`, commit `2970e5699ba6267f3384ffab20f96647578aebc8`,
with its submodule sources. The script pins the original and Debian archives
by SHA256 and applies Debian's patch series. A local DSC patch pins the iSCSI
default-value array to four bytes, removing inferred-size variation between
the DEC default `"yes"` and the component override `"no"`. The runtime value
remains `"no"`. The Python hash seed is fixed. Downloads are cached under
`guest/.cache/edk2/`. The fixed build epoch is the upstream commit's timestamp.

The build follows Debian's `debian/rules` for `OvmfPkg/OvmfPkgX64.dsc`,
X64, GCC, RELEASE, and 4 MiB flash. Both images enable CC measurement,
HTTP boot, IPv6, TLS, TPM2, and Debian's `UninstallMemAttrProtocol` shim workaround.
The secure-boot image additionally enables secure boot, requires SMM, and excludes
the UEFI shell. Firmware vendor and version identify vmlab. The build replaces
the logo asset and those identification strings relative to this configuration.
The iSCSI array-size patch changes data padding, not firmware policy.

Sources:

- https://deb.debian.org/debian/pool/main/e/edk2/edk2_2026.08+ds-2.dsc
- https://github.com/tianocore/edk2/tree/2970e5699ba6267f3384ffab20f96647578aebc8

## Variable-store compatibility

Each CODE image is 3,653,632 bytes. The VARS region is 540,672 bytes, making
4,194,304 bytes together. The flash starts at `0xffc00000`; CODE starts at
`0xffc84000`. No flash-layout PCD is changed. The build compares its generated
blank VARS byte for byte with the pinned Debian template and refuses a mismatch.
It discards the generated store and ships Debian's blank and Microsoft-enrolled
templates unchanged. Existing per-VM VARS retain their keys and boot entries.

AAVMF remains Debian's pinned build. The build container has no AArch64 firmware
cross compiler, and this change has no AArch64 guest with an existing VARS store
for compatibility validation. Branding it requires that toolchain and boot check.

## Replace the logo

The source and rendering command are documented in `logo/README.md`. Rebuild the
guest assets after changing the rendered BMP. No firmware lookup names change.

## Required vmlab-hv follow-up

The pinned protocol `41e53e474b537e6d6de48a34ad55c317d8d95ab7` has no
arbitrary fw_cfg file field in `MachineDescription` or `Boot::Firmware`.
The hypervisor needs a typed splash path and delay, or extra fw_cfg files, passed
through its VMM builder to `FwCfg::add_file`. SeaBIOS needs `bootsplash.jpg` and
the little-endian 16-bit `etc/boot-menu-wait` delay. No hypervisor source is changed
here. OVMF needs no such protocol extension.

## Verified outputs

The 2026-11-02 build uses GCC 13.3.0, binutils 2.42, NASM 2.16.01, and
iasl 20230628 in the Ubuntu 24.04 build container.

| File | SHA256 |
| --- | --- |
| `OVMF_CODE_4M.fd` | `17c3df4323e255cb0ce7d9ae185500c40445978d63ff5222223ce36dfd4c59cf` |
| `OVMF_CODE_4M.secboot.fd` | `b443691b95c921c18d32cf6e3ddf21271e276f29fa08b3bf1d2243f3ee99150c` |
| `OVMF_VARS_4M.fd` | `5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e` |
| `OVMF_VARS_4M.ms.fd` | `42ca3d90cbabb32c797f0253c26bead4bee9790f18cccf43993a88fa033ffa29` |

Ubuntu 24.04.4 boots under QEMU and vmlab-hv with a copy of an existing
540,672-byte VARS file. Its BGRT status is valid, and its BGRT bitmap matches
`logo/Logo.bmp` byte for byte. An existing Windows 11 template clone boots under
QEMU with its copied VARS and TPM state, reports `Confirm-SecureBootUEFI = True`,
and displays the wordmark with the Windows spinner. QEMU SeaBIOS displays the
JPEG splash. The source clones' VARS hashes and disk modification times remain
unchanged. Installed firmware and the owner's lab VMs are not modified.
