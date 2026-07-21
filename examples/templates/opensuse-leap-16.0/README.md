# openSUSE Leap 16.0 template

Builds `x86_64/opensuse-leap` into the local template store from the official
openSUSE Leap 16.0 **Minimal-VM cloud image** + cloud-init. Leap 16.0 is the
current stable line (24 months support per minor release); it supersedes the
now-EOL Leap 15.6.

```sh
vmlab validate
vmlab template build
vmlab template list      # → x86_64/opensuse-leap@16.0
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
`sha256` (from the `.sha256` beside the image under
<https://download.opensuse.org/distribution/leap/16.0/appliances/>).

The profile is `linux-modern` with `firmware = "seabios"` — cloud images
boot via BIOS.
