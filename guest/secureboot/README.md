# Microsoft Secure Boot template

## Exact Debian inventory

`debian-inventory.json` lists every PK, KEK, db and dbx entry in the pinned
Debian `ovmf-generic` 2026.08+ds-2 `OVMF_VARS_4M.ms.fd`. It includes certificate
subjects, DER SHA256 fingerprints, expiry dates, signature types and owners,
plus all 443 individual dbx image hashes. The original template's SHA256 is
`42ca3d90cbabb32c797f0253c26bead4bee9790f18cccf43993a88fa033ffa29`.

| Variable | Exact certificate subjects or entry set |
| --- | --- |
| PK | Debian UEFI Secure Boot (PK/KEK key), organization Debian, email debian-devel@lists.debian.org |
| KEK | The same Debian certificate; Microsoft Corporation KEK CA 2011; Microsoft Corporation KEK 2K CA 2023 |
| db | Microsoft Windows Production PCA 2011; Windows UEFI CA 2023; Microsoft Corporation UEFI CA 2011; Microsoft UEFI CA 2023; Microsoft Option ROM UEFI CA 2023 |
| dbx | 443 SHA256 image revocations, signature type c1c41626-504c-4092-aca9-41f936934328; no certificate or SVN revocations |

The Debian certificate's owner is `a0baa8a3-041d-48a8-bc87-c36d121b5e3d`.
All Microsoft entries have owner `77fa9abd-0359-4d32-bd60-28f4e78f784b`.
The full distinguished names and fingerprints are in the JSON inventory.
Debian already includes every requested 2023 certificate; expiry does not
explain a missing certificate in this particular template.

## Default and retained templates

`fetch-firmware.sh x86_64` retains the original as
`OVMF_VARS_4M.ms.debian.fd`, then runs `build-secureboot.sh`. The default
`OVMF_VARS_4M.ms.fd` preserves every original entry and adds the current
compatible Microsoft x64 dbx. Both files remain 540,672 bytes. No PK, KEK or db
entry needs adding to this Debian input; all seven official Microsoft
certificates are verified and enrolled idempotently by the FW6 tool.

`microsoft-2023.json` pins the official certificate URLs and DER SHA256 hashes,
Microsoft release v1.7.1, its source commit
`daa32713651f2ffe97b18df5aaf93ffe238aa464`, the x64 release archive SHA256,
the dbx member SHA256 and the license. This is the latest published release
checked on 2026-11-02, published on 2026-10-20. Its compatible dbx has 445
SHA256 entries. The two additions to Debian's set are:

```text
2ea557c44b83c0ad6b71efb7edcc18b6337ad1c1d682155dd9451b051b62ff40
86a9482509a434ca61ea4d12b6010a9013e5d3fbff09b14799ebc87c124bcff0
```

This uses `LegacyFirmwareDefaults/Firmware/DefaultDbx.bin`, not Microsoft's
optional PCA/SVN revocations. It does not revoke either retained 2011 CA.
The default template SHA256 is
`dd4e1c140f74bbc0ef40caa6b7965be04a198a92733a8808d0080a53f388b32d`.
`SECUREBOOT-BUILDINFO.json` in the bundle records the complete resulting
inventory, source pins, tool/lock hashes and the Microsoft BSD-2-Clause-Patent
license. AArch64's template is unchanged by this x86_64 follow-up.

Official release and input documentation:

- https://github.com/microsoft/secureboot_objects/releases/tag/v1.7.1
- https://github.com/microsoft/secureboot_objects/tree/daa32713651f2ffe97b18df5aaf93ffe238aa464
- https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/windows-secure-boot-key-creation-and-management-guidance

## Rebuild or inspect

The build requires uv and the adjacent locked Python dependencies. The guest
toolchain's `uv` stage installs a pinned runner; the release build container
and guest-assets CI job include it. From the repository root:

```sh
bash guest/fetch-firmware.sh x86_64
uv run --frozen --script guest/enroll-secureboot.py \
  --inspect guest/dist/firmware/x86_64/OVMF_VARS_4M.ms.fd
```

To create a separate candidate without replacing either bundle output:

```sh
uv run --frozen --script guest/enroll-secureboot.py \
  --template guest/dist/firmware/x86_64/OVMF_VARS_4M.ms.debian.fd \
  --microsoft-manifest guest/secureboot/microsoft-2023.json \
  --output candidate-vars.fd
```

Microsoft mode checks the input against the pinned pristine Debian SHA256.
It refuses modified guest stores and existing outputs. Downloaded objects
are size-bounded and hash-verified before parsing. Enrollment preserves
original signature entries, authenticated-variable metadata, other variables
and flash layout. It reparses the output and verifies these invariants.
Identical inputs produce identical bytes and build reports, including across
the host and Ubuntu 24.04 build container.

To select the old template for a new VM, set `secure_boot = true` and
`secure_boot_vars` to the retained `OVMF_VARS_4M.ms.debian.fd` in your asset
directory. Use an absolute path when the asset directory is outside the lab.
Do not also set `secure_boot_keys`. The standard custom-VARS path copies it
once; it never replaces an existing VM's store. Both hypervisors support it.

## Service an existing VM

A guest keeps `.vmlab/vms/<vm>/OVMF_VARS.fd` after first start. Installing
new CODE or bundled VARS does not enroll new keys into that store. Do not
delete it or replace it with a fresh template to update certificates. Doing
so loses boot entries, custom trust and accumulated revocations, and can
change TPM measurements and trigger BitLocker recovery.

1. Before servicing, preserve a recoverable, consistent offline backup of the
   VM's disks, VARS and TPM state. Have current recovery media and any BitLocker
   recovery key available. Follow Microsoft's protection-suspension guidance
   when a servicing step requires it.
2. For Windows, apply supported Windows updates and Microsoft's Secure Boot
   certificate servicing procedure. Windows can apply authenticated 2023 db
   updates itself on eligible systems. Check servicing status before deploying
   boot components signed only by a 2023 CA; enabling Secure Boot alone proves
   neither enrollment nor successful servicing.
3. Inspect `Confirm-SecureBootUEFI`, `Get-SecureBootUEFI -Name db` and
   `Get-SecureBootUEFI -Name KEK` in an elevated PowerShell session. Parse the
   returned EFI signature lists and check the actual certificate fingerprints.
   The new KEK requires a PK-authorized update, not a db update. The bundled
   store uses Debian's PK; vmlab has no private PK and supplies no signed KEK
   rotation payload. A PK-authenticated platform update or an explicit owner-
   controlled firmware enrollment process is required where KEK 2023 is absent.
4. For Linux, use distribution-supported shim/grub updates and the supported
   fwupd trust/revocation update channel. Verify enrolled CAs and revocation
   compatibility before switching to a 2023-only signed shim.

No vmlab-side refresh command is provided. The offline build tool is not an
authenticated runtime updater. Unconditionally replacing a guest store cannot
preserve its authenticated state safely.

Expiry alone does not stop an existing trusted, unrevoked boot chain from
booting. Microsoft states that Windows devices can continue to start and
receive standard updates while losing future early-boot security protections.
A firmware db containing only 2011 CAs cannot validate an image signed only
by a 2023 CA. Revocation of a particular old loader can also stop that loader,
independently of CA expiry. Retaining both CA generations preserves trust
compatibility but does not override dbx or shim SBAT revocations.

Servicing and expiry sources:

- https://support.microsoft.com/en-us/servicing/os/secure-boot/2026/02/when-secure-boot-certificates-expire-on-windows-devices
- https://learn.microsoft.com/en-us/troubleshoot/windows-client/windows-security/update-secure-boot-certificates
- https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/linux-distribution-guidance-handling-2023keys
- https://github.com/microsoft/secureboot_objects/blob/daa32713651f2ffe97b18df5aaf93ffe238aa464/scripts/information/signed_binaries_information.md
