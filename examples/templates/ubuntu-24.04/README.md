# Ubuntu Server 24.04 template

Builds `x86_64/ubuntu-24.04` into the local template store from the
official live-server ISO (downloaded and sha256-verified automatically)
using a subiquity autoinstall config delivered on a CIDATA ISO.

```sh
vmlab validate
vmlab template build
vmlab template list      # → x86_64/ubuntu-24.04@24.04.4
```

What happens (`scripts/install.ws` narrates it in the build log):

1. The build VM boots the installer ISO with `cloudinit/` attached as a
   `CIDATA` volume (NoCloud datasource).
2. The script answers subiquity's "Continue with autoinstall?" prompt via
   OCR + keystrokes.
3. Autoinstall partitions the disk, creates user `vmlab` (password
   `vmlab`), installs the vmlab guest agent (from the auto-attached VMLAB ISO), and powers the VM off.
4. vmlab flattens and seals the disk into the store.

Point-release bumps: update `version`, the ISO filename, and `sha256`
(from <https://releases.ubuntu.com/24.04/SHA256SUMS>) in `vmlab.wcl`.
