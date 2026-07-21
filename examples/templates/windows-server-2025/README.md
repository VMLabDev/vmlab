# Windows Server 2025 (Evaluation) template

Builds `x86_64/windows-server-2025` into the local template store from the
Microsoft Evaluation Center ISO (downloaded and sha256-verified
automatically), fully unattended via `unattend/autounattend.xml`.

```sh
./fetch-deps.sh          # one-time: virtio drivers + guest MSIs into unattend/
vmlab validate
vmlab template build     # ~30-45 minutes
vmlab template list      # → x86_64/windows-server-2025@26100.1742
```

What happens (`scripts/install.ws` narrates it in the build log):

1. The build VM boots the eval ISO; the script types through the
   "Press any key to boot from CD or DVD" prompt (and resets + retries if
   the firmware shell appears instead).
2. `autounattend.xml` (on the UNATTEND ISO built from `unattend/`) loads
   the virtio storage/net drivers in WinPE, partitions UEFI/GPT, installs
   **Standard Evaluation (Desktop Experience)**, sets `Administrator` /
   `vmlab123!`, and on first logon installs the virtio guest tools and the
   vmlab guest agent (from the auto-attached VMLAB ISO).
3. The script waits for the guest agent — that's the "install finished"
   signal — then vmlab shuts the VM down and seals the disk.

Notes:

- `fetch-deps.sh` stages redistributable binaries (virtio drivers, MSIs)
  into `unattend/`; they are gitignored, only the answer file is tracked.
- The evaluation license runs 180 days from install. To move to a newer
  eval build, update `version`, the `url`, and `sha256` in `vmlab.wcl`
  (sha256: download the ISO once and `sha256sum` it).
- All lab clones share the machine name `VMLAB-WS2025`; sysprep is out of
  scope for this example.
