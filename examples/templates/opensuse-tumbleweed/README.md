# openSUSE Tumbleweed template

Builds `x86_64/opensuse-tumbleweed` into the local template store from the
official Tumbleweed **Minimal-VM cloud image** + cloud-init. Tumbleweed is
the rolling-release counterpart to Leap (the current/bleeding-edge option).

```sh
vmlab validate
vmlab template build
vmlab template list      # → x86_64/opensuse-tumbleweed@20260613
```

What happens (`scripts/install.ws` narrates it in the build log):

1. The build VM boots the cloud image with `cloudinit/` attached as a
   `CIDATA` volume (NoCloud datasource).
2. cloud-init creates user `vmlab` (password `vmlab`, passwordless sudo),
   installs the vmlab guest agent (from the auto-attached VMLAB ISO), enables it, and disables cloud-init for
   subsequent boots.
3. The script waits for the guest agent to respond and cloud-init to finish.
4. vmlab powers the VM off and seals the disk into the store.

Tumbleweed rolls, so this pins a dated snapshot. To refresh, pick a newer
`Cloud-Snapshot` image (and its `.sha256`) from
<https://download.opensuse.org/tumbleweed/appliances/> and update `url`,
`sha256`, and `version`.

The profile is `linux-modern` with `firmware = "seabios"` — cloud images
boot via BIOS.
