# AlmaLinux 10 template

Builds `x86_64/almalinux-10` into the local template store from the official
AlmaLinux **GenericCloud** image + cloud-init. AlmaLinux and Rocky are the
two community RHEL rebuilds; this pairs with the `rocky-9` template.

```sh
vmlab validate
vmlab template build
vmlab template list      # → x86_64/almalinux-10@10.2
```

What happens (`scripts/install.ws` narrates it in the build log):

1. The build VM boots the cloud image with `cloudinit/` attached as a
   `CIDATA` volume (NoCloud datasource).
2. cloud-init creates user `vmlab` (password `vmlab`, passwordless sudo),
   installs the vmlab guest agent (from the auto-attached VMLAB ISO), enables it, and disables cloud-init for
   subsequent boots.
3. The script waits for the guest agent to respond and cloud-init to finish.
4. vmlab powers the VM off and seals the disk into the store.

Point-release bumps: update `version`, the image filename in `url`, and
`sha256` (from the `CHECKSUM` beside the image under
<https://repo.almalinux.org/almalinux/10/cloud/x86_64/images/>).

The profile is `linux-modern` with `firmware = "seabios"` — cloud images
boot via BIOS. Note RHEL 10 requires an x86-64-v3 host CPU.
