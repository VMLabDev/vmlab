# Fedora 44 template

Builds `x86_64/fedora-44` into the local template store from the official
Fedora **Cloud Base** image + cloud-init. Fedora has no LTS — this is the
current release (a new one lands roughly every six months).

```sh
vmlab validate
vmlab template build
vmlab template list      # → x86_64/fedora-44@44.1.7
```

What happens (`scripts/install.ws` narrates it in the build log):

1. The build VM boots the cloud image with `cloudinit/` attached as a
   `CIDATA` volume (NoCloud datasource).
2. cloud-init creates user `vmlab` (password `vmlab`, passwordless sudo),
   installs the vmlab guest agent (from the auto-attached VMLAB ISO), enables it, and disables cloud-init for
   subsequent boots.
3. The script waits for the guest agent to respond and cloud-init to finish.
4. vmlab powers the VM off and seals the disk into the store.

Version bumps (~every 6 months): update `version`, the image filename in
`url`, and `sha256` (from the `Fedora-Cloud-NN-x.y-x86_64-CHECKSUM` beside
the image under
<https://download.fedoraproject.org/pub/fedora/linux/releases/>).

The profile is `linux-modern` with `firmware = "seabios"` — cloud images
boot via BIOS.
