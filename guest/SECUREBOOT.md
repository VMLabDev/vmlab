# Add lab signers alongside Microsoft's keys

The default VARS remains Debian's Microsoft-enrolled template. Enrollment is
optional and offline. It needs only public X.509 certificates; keep signing
private keys outside this repository and its firmware bundle. No production
vmlab signing certificate is included until the owner supplies that identity.

From a source checkout, create a new combined template:

```sh
uv run --frozen --script guest/enroll-secureboot.py \
  --certificate /path/to/vmlab-public.pem \
  --output keys/OVMF_VARS_4M.vmlab.fd
```

Create the output's parent directory first. Repeat `--certificate` for multiple
signers. The default input is the built
`guest/dist/firmware/x86_64/OVMF_VARS_4M.ms.fd`; `--template` can name another
Microsoft-enrolled x86_64 template. The script requires uv and uses the adjacent
dependency lock. PEM and DER certificates are accepted. `--owner` optionally
sets the signature-owner GUID; otherwise each certificate's fingerprint defines
a stable owner GUID.

The tool appends certificates to `db`. It preserves Microsoft db entries,
PK, KEK, dbx, all other variables and the 540,672-byte layout, and verifies
those invariants by reparsing the result. Identical input and certificates
produce identical output. The output must not exist; an existing VM's VARS
is never replaced. The JSON result records public certificate and file hashes.
This is not an authenticated live update or a key-rotation mechanism.

Select that template on a new VM:

```wcl
vm "signed-lab" {
  template = "x86_64/ubuntu-24.04"
  firmware = "ovmf"
  secure_boot = true
  secure_boot_vars = "keys/OVMF_VARS_4M.vmlab.fd"
}
```

Do not also set `secure_boot_keys`. The existing custom-VARS path copies the
file at first start; subsequent boots keep the VM's store. Template builds can
use the same field, carrying the combined store with their clones. Adding a
signer to this offline template does not update machines that already exist.

Run the focused enrollment checks after building/fetching x86_64 firmware:

```sh
PYTHONDONTWRITEBYTECODE=1 uv run --with virt-firmware==26.9 \
  python -m unittest discover -s guest/tests -v
```

The tests use ephemeral identities. Runtime validation additionally boots a
test-signed EFI application under the combined store with `SecureBoot=1`,
rejects an unsigned application under that store, and rejects the test-signed
application under Microsoft-only VARS. Microsoft signatures remain trusted;
adding a lab certificate broadens the accepted signing identities.

The pinned OVMF permits unsigned PCI EFI option ROMs, including iPXE, by its
explicit option-ROM policy. This does not exempt a downloaded EFI boot payload
from signature checking. A lab needing signed option ROM enforcement must
also change that policy and arrange ROM signing, or use native edk2 networking.

Tool source and behavior: https://gitlab.com/kraxel/virt-firmware
