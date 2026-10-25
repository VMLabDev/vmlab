# Troubleshooting

Each section starts with the message vmlab prints, then the cause, then the fix. Parts in braces are filled in with a machine, lab or path name. Most messages reach the lab daemon's log as well as the terminal, so `vmlab logs` shows the same words when a command has already returned. Non-zero exit codes are the protocol error codes.

## KVM is unavailable and the guest runs under TCG

```
{machine}: KVM unavailable for {arch} — falling back to TCG (slow)
```

This is a warning, not an error. The machine still boots, with `-accel tcg`, and every instruction is emulated. Boot takes minutes rather than seconds and provisioning runs at the same pace.

vmlab uses KVM only when it can open `/dev/kvm` for reading and writing and the guest's architecture matches the host's. Two causes:

- The daemon user cannot open the device (the device is absent, or the user is not in the `kvm` group).
- The lab asks for a foreign architecture, as `examples/alpine-arm64` and `examples/riscv64-ubuntu` do on an x86 host. This case is expected and has no fix.

For the first case, add your user to the `kvm` group, log in again, then restart the daemons with `vmlab lab restart`. On WSL 2 see "Nested virtualisation is off on WSL 2" below.

## A runtime binary is missing

```
missing required binaries on PATH: {names} — install the QEMU/swtpm packages (PRD §14 lists the runtime dependencies)
```

`vmlab up` and `vmlab machine start` check the binaries every targeted machine needs before anything boots. A VM needs `qemu-img` and the `qemu-system-<arch>` for its architecture, and `swtpm` when its resolved hardware has a TPM. A lab container needs `qemu-img` and the host architecture's `qemu-system`. `vmlab validate` does not run this check, so a lab that validates can still fail here.

Install the packages and run `vmlab up` again. Three more dependencies are not in this preflight and fail later, at the point of use, with their own words.

| Message | Cause | Fix |
| --- | --- | --- |
| `{arch} UEFI firmware not found; tried: {paths}; vmlab's own firmware was not found either …` | The guest assets carrying vmlab's bundled OVMF/AAVMF are missing from every guest asset directory, and the host has no distro firmware to fall back to. riscv64 has no bundled firmware. | Reinstall the guest assets (`install.sh`, or `just guest-install`), or point `firmware_dir` in the host config at a copy. riscv64: install `qemu-efi-riscv64`. |
| `{arch} UEFI VARS template not found; tried: {paths}` | Host fallback only: distro firmware code found, its variable-store template not. | Reinstall the guest assets, or install the same firmware package. |
| `{arch} secure boot needs a UEFI VARS template with keys enrolled …` | `secure_boot = true`, the bundled firmware is missing, and the host has a secure-boot build but no enrolled variable store beside it (Arch's `edk2-ovmf`). A blank store boots in setup mode and enforces nothing, so vmlab refuses it. | Reinstall the guest assets: they carry enrolled `OVMF_VARS_4M.ms.fd` (x86_64) and `AAVMF_VARS.ms.fd` (aarch64). |
| `firmware_dir {dir} is the {arch} firmware this host config chose, and it lacks …` | The host config's `firmware_dir` has a directory for this arch, making it the only place that arch is looked up, and the needed pair is not in it. | Add the named file, or remove the arch's directory to fall back to vmlab's own firmware. |
| `this VM's UEFI VARS store is {n} bytes and the firmware vmlab would boot ({code}) takes {m} …` | The VM was created under a firmware of another flash layout and no build of that layout is installed any more. | Delete `.vmlab/vms/<vm>/OVMF_VARS.fd` for a fresh store (losing UEFI boot entries), or reinstall the firmware it was created with. |
| `share "{share}" demands transport = "virtiofs" but no usable virtiofsd was found on this host …` | A share declares `transport = "virtiofs"` and no `virtiofsd` is on `PATH` or in the distribution's helper directories. | Install `virtiofsd`, or point `VMLAB_VIRTIOFSD` at one. |
| `… virtiofsd {version} ({path}) cannot serve a share: its --help lists no {flags} …` | The `virtiofsd` found lacks a flag every share needs (QEMU's retired C `virtiofsd`). | Install the Rust `virtiofsd`, 1.13.0 or later. |
| `share "{share}" is read-only, and virtiofsd {version} ({path}) has no --readonly (added in virtiofsd 1.13.0) …` | A read-only share demands virtiofs on a `virtiofsd` older than 1.13.0. | Install a newer `virtiofsd`, or set `transport = "smb"` or `"auto"`. |
| `{machine}: online snapshots of a machine with virtiofs devices need virtiofsd 1.11.0 or later …` | Online snapshot capture or restore on a machine whose virtiofs devices ride a `virtiofsd` without `--migration-mode` (Ubuntu 24.04's 1.10.0). | Use an offline snapshot, or install `virtiofsd` 1.11.0 or later and restart the machine. |
| `cannot spawn sqfstar` | A container image is being flattened and `squashfs-tools` is not installed. | Install `squashfs-tools`. |

A `secure_boot = true` machine whose firmware lookup fails is refused earlier, at hardware resolution, with `vm "{machine}": secure_boot = true (from {source}) but no firmware …`. Secure boot is enforced: a guest whose bootloader Microsoft's keys do not sign is refused by the firmware (`Access Denied` on its console).

## No guest asset for a container micro-VM

```
no micro-VM guest asset for {arch} (need vmlinuz + initramfs.img); searched: {dirs}. Build one with `guest/build-asset.sh {arch}` and install it into one of those directories (or point VMLAB_GUEST_ASSET_DIR at guest/dist).
```

A lab container boots a micro-VM from a kernel and initramfs that ship with vmlab, not with the image. vmlab looks for them under `$VMLAB_GUEST_ASSET_DIR/<arch>/`, then `/usr/share/vmlab/guest/<arch>/`, then `~/.local/share/vmlab/guest/<arch>/`. None of those held both files.

A packaged install puts the asset under `/usr/share/vmlab/guest`. From a source checkout, build it with `guest/build-asset.sh <arch>` and either copy `guest/dist` into one of the searched directories or export `VMLAB_GUEST_ASSET_DIR=guest/dist` for the daemons. The agent binary a template bake needs has the same search path and the same shape of message, naming `guest/build-agent.sh <os>-<arch>` instead.

## The template is not in the store

```
template {arch}/{name}@{version} not found in the store
```

The lab names a template by store ref and no version of it is installed, or the pinned version is not. The reply carries the `not_found` error code, so the command exits 4.

`vmlab template list` shows what the store holds. If the template is published on a registry, point the `template` field at its registry ref and `vmlab up` pulls it, or run `vmlab pull` first to fetch every missing template without booting. If it is a local build, run `vmlab template build` in the template's directory, as each directory under `examples/templates` shows. A store ref that exists but has lost its disk is a different message, `template {ref} is corrupt: missing disk.qcow2`; remove that version with `vmlab template rm` and pull or build it again.

A container whose image has not been fetched yet says ``{machine}: image not pulled yet — run `vmlab pull` or `vmlab up``. Both verbs pull it.

## The machine never becomes ready

```
{machine}: not ready after 600s
{machine} stopped while waiting for ready
no vmlab-agent answered on the agent channel
agent did not open the channel in time
```

A machine is *ready* when its agent answers on the `vmlab.agent.0` virtio-serial port. VMs get ten minutes, containers five. `vmlab up` waits for readiness only when a later wave depends on the machine, or a provision or share must run on it, so the first symptom is often `first-boot {name}: agent did not come up` from a provision rather than the timeout itself.

Usual causes, in order of likelihood: the template was built without the agent, or with an agent from an older vmlab; the guest is still installing (a template build that has not finished); the guest is running under TCG and has not reached userspace yet; or the guest booted but its agent service failed. Open the display with `vmlab console <machine>` and look. `vmlab machine capabilities <machine>` prints `agent -` when the agent has not answered at all, and a feature list when it has.

If the guest is up and the agent is absent, rebuild the template so the bake installs the agent this vmlab ships, or push one into the running machine with `vmlab machine repair-agent`. A container's agent comes from the host's guest asset, so on a container the message ends with `there is nothing to rebuild or repair — an agent that is not answering here is a machine to restart, or a guest asset to reinstall (§19.4)`.

## A copy failed because the file channel went silent

```
push C:/tools/driver.sys: writing at offset 65536: the guest's agent answered nothing for 60s: the write at offset 65536 is unanswered and nothing else moved on the file channel
pull C:/big.bin: reading at offset 63569920: channel lost: port connection reset (…)
```

Every request on the agent's file session runs under a progress watchdog: the first message means nothing at all arrived from the guest for 60 seconds while the named request was outstanding. It is silence, not slowness — a slow transfer that is still moving renews the budget with every reply. The second is the guest's own report: its port connection was reset under the session (a port write that pended more than 15 seconds, which happens when the host or QEMU stopped draining the port under load), bytes were lost, and it failed every open channel rather than let one wait. Both name the file; rerun the copy. The lab daemon log carries the same words. An agent older than vmlab 0.31 dropped frames silently on that reset, which left the copy waiting forever — `vmlab machine repair-agent <machine>` replaces it.

## Autologon set on a fresh Windows clone is gone after a reboot

```
AutoAdminLogon=
DefaultUserName=
DefaultPassword set=True
```

vmlab prints nothing. A lab writes `AutoAdminLogon`, `DefaultUserName` and `DefaultPassword` under `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon` on a fresh Windows 11 clone and reboots; afterwards `AutoAdminLogon` and `DefaultUserName` are gone, `DefaultPassword` survives, nobody is logged on. Intermittent.

Cause: Windows' OOBE, replayed on every clone's first boot. `msoobe.exe` enables autologon for its temporary `defaultuser0`; `CloudExperienceHostBroker.exe` disables it when user OOBE exits, deleting `AutoAdminLogon`, `DefaultUserName` and `SystemAutoLogon` whoever wrote them. A template first-boot script that reports ready, or reboots the clone, before that exit lets user OOBE resume on the next boot and delete the lab's values. `C:\Windows\Panther\UnattendGC\setupact.log` shows `Clearing Auto-logon values per request` stamped after the lab set them. Seen on windows-11 clones; any Windows template whose first-boot script predates the vmlab-templates fix can have it.

Fix: rebuild or re-pull the template once a fixed version is published (its first boot holds `ready` until user OOBE exits). On an older clone, run this until it prints `done`, then set autologon and reboot:

```powershell
$s = Get-ItemProperty HKLM:\SYSTEM\Setup
$w = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
$l = Select-String C:\Windows\Panther\UnattendGC\setupact.log `
       -Pattern 'fEnableAutologon = 1|Clearing Auto-logon values' -ErrorAction SilentlyContinue |
     Select-Object -Last 1
if ($s.SystemSetupInProgress -ne 0 -or $s.OOBEInProgress -ne 0 -or $w.SystemAutoLogon -or
    ($l -and $l.Line -notmatch 'Clearing')) { 'waiting' } else { 'done' }
```

A leftover `AutoLogonCount` is a different cause with the same symptom (Windows counts it down per autologon and clears the logon at zero); remove it when setting the values.

## A shared app is refused

`vmlab app run` refuses before any window opens; cli-machine.md (`vmlab app`) has the full refusal table. The common ones:

| Message | Cause | Fix |
| --- | --- | --- |
| `no graphical session: neither WAYLAND_DISPLAY nor DISPLAY is set` | The command ran outside a desktop session (over SSH, a virtual console, a service). | Run it from a terminal inside the desktop session. |
| ``` no FreeRDP 3 on this host: shared apps from a Windows guest need `xfreerdp3` or `xfreerdp` on PATH, or `freerdp` in config.wcl ``` | A Windows guest's host end is FreeRDP, and none was found. | Install FreeRDP 3 (`xfreerdp3` on Arch and Debian, `xfreerdp` on Fedora), or set `freerdp` in the host config. |
| ``` `{machine}` has no `waypipe` on PATH for `{login}` ``` | The guest has no `waypipe` for the login (`{login}` may read `the agent identity`), and the host has no `DISPLAY` to fall back to X11 on. With `DISPLAY` set the same words end in `; falling back to X11 through Xwayland` and the session continues. | Install `waypipe` in the guest on that login's `PATH`, e.g. `vmlab exec --user root <machine> -- apk add waypipe`. |
| ``` the agent on `{machine}` predates shared apps over `{mode}`; run `vmlab machine repair-agent {machine}` ``` | The agent advertises no `app_waypipe`/`app_x11`/`app_rdp`. The legacy agent tier never does. | `vmlab machine repair-agent <machine>`, or rebuild the template (see the section below). |
| ``` {machine}: Windows `{edition}` has no Remote Desktop host; shared apps need Pro, Enterprise, Education or Server ``` | `EditionID` is a Home edition (`Core`, `CoreN`, `CoreSingleLanguage`, `CoreCountrySpecific`). | Build the template from a Pro, Enterprise, Education or Server image. |
| ``` RDP needs an account: declare a `login {}` on `{machine}` or pass --user/--password ``` | RDP cannot log on as SYSTEM; the machine has no `login {}` and no `--user` was given, or `--user SYSTEM` was. | Declare a `login {}` with a `password` (logins.md), or pass `--user` and `--password`. |

## A recording is refused

| Message | Cause | Fix |
| --- | --- | --- |
| ``` recording needs ffmpeg: none found on PATH, and the host config (config.wcl) names none under `ffmpeg` ``` | The lab daemon looks for ffmpeg only when a recording starts, and found none. Exit 1 (`failed`). | Install ffmpeg, or set `ffmpeg` in the host config (host-profiles.md) to the binary. |
| ``` recording a .mp4 needs the `libx264` encoder, which {ffmpeg} -encoders does not list; install an ffmpeg built with it, or point the host config's `ffmpeg` key at one ``` | The ffmpeg found cannot write the format: `.mp4` needs `libx264`, `.webm` needs `libvpx-vp9`. Exit 1. | Install an ffmpeg built with the encoder, or point the `ffmpeg` key at one. |
| ``` machine `{machine}` is already recording to {path}; stop it first (`vmlab record stop {machine}`) ``` | One recording per machine. Also meets `vm restart --record` of a VM already recording (its recording follows the restart on its own). Exit 5 (`conflict`). | `vmlab record stop {machine}`, then start again; `vmlab record status` lists what is live. |
| ``` machine `{machine}` has no display ``` | Only a VM has a framebuffer; a container cannot be recorded. Exit 6 (`unsupported`). | Nothing changes that. |
| ``` machine `{machine}` did not start, so its recording was not started ``` | A `--record` on `up`/`vm start`/`vm restart` named a VM whose boot failed. Exit 1. | Fix the boot; a VM that was already running would have been recorded from now instead. |
| ``` recording `{path}`: unsupported extension `.gif`; a recording is written as `.mp4` (H.264) or `.webm` (VP9) — convert it afterwards for anything else ``` | Refused by the CLI before any daemon is reached. | Record to `.mp4` and convert with ffmpeg afterwards. |

## `vmlab machine repair-agent` refused

A machine whose agent is older than the host's lacks features the host expects; `vmlab machine capabilities <machine>` lists the ones it negotiated. `vmlab up` and `vmlab vm start` normally refresh it by themselves when the VM's agent stamp differs from the shipped agent's, printing `agent: updated "<vm>" (<old> → <new>)`, or `warning: agent: could not update "<vm>" …` with the reason, leaving the old agent in place and carrying on; `agent_update = false` on the `vm` or `lab` block turns that off. Rebuilding the template is the durable remedy. `vmlab machine repair-agent <machine>` pushes the host's shipped agent into the running machine over its own channel and marks the machine *diverged* in `vmlab status`, so you remember the clone no longer matches its template. It refuses on three conditions:

| Refusal | Meaning |
| --- | --- |
| `"{machine}" must be running with its agent answering before a new one can be pushed into it over that channel` | There is no channel to push over. Start the machine, or fix the readiness problem first. |
| ``"{machine}"'s agent serves no `fileops`, so it cannot be handed a binary over its own channel — this one can only be replaced by rebuilding the template (§19.4)`` | The old agent cannot receive a file. Rebuild the template. |
| `this machine's agent lives in the initramfs guest asset this host installed, not in anything it boots — it already tracks the vmlab you are running and cannot go stale, so there is nothing to push into it. Refreshing it means reinstalling the guest asset (§19.4)` | The machine is a container. Reinstall the guest asset instead. |

## The workspace has stopped syncing

```
the workspace on "{machine}" has stopped, both directions, on {n} conflicting paths
```

The workspace syncer found a path changed on both sides since they last agreed, and halted the whole workspace on that machine, in both directions. It wrote nothing and deleted nothing: both copies are exactly where they were. The watch keeps running, so the halt lists every conflicting path in the batch rather than the first one. In the guest, a file named `.vmlab-sync-halt` at the workspace root carries the same list; it is the only signal the guest side gets, and it never syncs.

Resolution is host-side, from the lab directory, because vmlab opens every channel from the host and the guest only answers. `vmlab dev sync status` prints the halt with every path and the reason for each. Then, per path or for the batch: `vmlab dev sync diff <path>` shows the guest copy next to the host copy; `vmlab dev sync resolve <path> --host` keeps the canonical host copy and overwrites the guest's; `--guest` does the reverse; and `--all` takes every halted path with the flag you give. Making both sides identical by hand is a third route that needs no verb: the next pass adopts them as agreed. `resolve` with neither side flag refuses with `say which side wins`, and with no path and no `--all` it refuses with ``name the paths to resolve, or pass `--all` to take the whole batch``.

Warning: `resolve` overwrites the side that loses and vmlab keeps no copy of it. Run `vmlab dev sync diff` first when either side might hold something you want.

Two other messages come from the same loop and look like a halt but are not:

- Bulk-delete guard: `the guest deleted {n} of the {m} paths this workspace had agreed on, which is a rewrite of the canonical copy rather than an edit: nothing was removed on the host`. Halts on a delete batch above a proportion with a floor; resolve it with `--guest` if the deletes were intended.
- Volume warning: ``this pass is carrying {n} paths ({m} MiB) under {prefix} — syncing continues, and adding `{prefix}/` to .vmlabignore makes that subtree guest-owned if it is build output``. Never halts.
- A line under `deferred while git holds a lock` is timing, not a conflict, and clears itself when the lock goes.

## A snapshot capture is refused

```
"{machine}"'s workspace is not in step with the canonical copy, so this snapshot would capture a tree the host has never agreed with. … There is no flag for this — a snapshot of a tree mid-transfer restores to somewhere meaningless. `vmlab dev sync status {machine}` says what is outstanding and `vmlab dev sync flush {machine}` waits for it; a halt has to be resolved first. Snapshots are not a workspace backup: a dev machine's source lives on the host, which is what survives `destroy` and what a restore re-converges the guest from.
```

Snapshot capture on a dev machine flushes the syncer first and refuses while the guest holds work the canonical copy has never seen: unsynced paths, a halt, an unfinished re-seed, or a workspace that has not completed a single pass. There is no escape flag, by design. Run `vmlab dev sync flush <machine>` and take the snapshot again; if the reason is a halt, resolve it as above. The middle of the message says which of these it is and names up to twenty of the paths still owed.

Restore has a matching refusal when the workspace is halted: `"{machine}"'s workspace is halted, and restoring would silently destroy the guest copy of every conflicting path.` Restore does have an escape flag, `--discard-guest-changes`, which throws away the guest copy of the whole workspace and re-converges it from the host after the rewind. Both refusals ride the ledger, so they apply to a stopped machine too.

## A USB device node cannot be opened

```
cannot open /dev/bus/usb/{bus}/{dev} for {vid}:{pid} {device} at port {port} — vmlab runs as {user} and needs read-write access to it.
  for now:   sudo setfacl -m u:{user}:rw /dev/bus/usb/{bus}/{dev}
  for good:  a udev rule, e.g. SUBSYSTEM=="usb", ATTR{idVendor}=="{vid}", ATTR{idProduct}=="{pid}", TAG+="uaccess" in /etc/udev/rules.d/70-vmlab-usb.rules
```

`vmlab usb attach` hands the node to QEMU, which runs as the user, and a stock node is `root:root 0664`. The `setfacl` grant lasts until the device is replugged (the device number in the path changes) or the host reboots. The udev rule lasts: write it, `sudo udevadm control --reload && sudo udevadm trigger`, replug. Then attach again.

## A VM has no USB controller to attach to

```
"{machine}" has no USB controller to attach to — set `usb = true` on {machine} and restart it
```

Host USB devices hot-plug onto an xHCI the VM gets only when it starts with `usb = true` (vm block or profile). Add it and `vmlab vm restart {machine}`. Off by default because it changes the VM's hardware: online snapshots taken before no longer restore. A container refuses with `USB passthrough is for VMs` and nothing changes that.

## A port forward was skipped

```
{machine}: {forward}: host port {port} is already claimed by {machine}: {forward}
```

Every forward in the lab is planned before `up` installs any. When two claim the same host port, the first in plan order wins and the rest are dropped rather than left to a bind failure, because a bind failure names neither the winner nor the fact that there was a contest. The same plan skips a forward for three other reasons, each printed as `{what}: {why}`.

| Reason | Meaning |
| --- | --- |
| `no such vm or container in the lab` | The forward's `to` names a machine the lab does not declare. |
| `needs a nic to reach it over` | The target machine has no `nic {}`, so no segment reaches it. |
| `no lease — is it running and ready?` | The target has a NIC but no DHCP lease yet. The forward installs once the machine is up; `vmlab status` shows the lease. |

A host port held by an unrelated process is not detected by the plan. That forward fails at install time, which `up` treats as best effort and reports in the lab log. Change the `host_port` or free the port. Two instances of one lab file declare the same fixed `host_port`, so the second instance's forward fails this way while the first holds the port. Omit `host_port` (or a `port {}`'s `host`) and each instance binds a free port of its own, which `vmlab status` shows marked `(auto)`.

## Nested virtualisation is off on WSL 2

vmlab prints nothing specific to WSL 2. When nested virtualisation is disabled in `.wslconfig`, `/dev/kvm` does not open and every machine gets the TCG warning at the top of this file. Enable it under `[wsl2]` in `%USERPROFILE%\.wslconfig`, run `wsl --shutdown` from Windows, and start again. See host-profiles.md for the full setup.

Some WSL 2 sessions have no `$XDG_RUNTIME_DIR`. vmlab does not fail on that: it falls back to `/tmp/vmlab-<uid>` for the supervisor socket and other runtime files, creates it with mode 0700, tightens an existing directory, and refuses one owned by another user. If the sockets are under `/tmp` rather than `/run/user/<uid>`, that is why. Networking on WSL 2 needs no tap, bridge or macvlan, so the fast path degrades to userspace silently there; a Windows-side VNC viewer is reached with `vmlab console --tcp`, which bridges the display to a localhost port.

## The supervisor did not come up

```
supervisor did not come up — check ~/.local/state/vmlab/vmlabd.log
```

Every `vmlab` verb that needs the supervisor connects to `$XDG_RUNTIME_DIR/vmlab/vmlabd.sock`, spawns `vmlabd` if nothing answers, and retries for about ten seconds. This message means the spawned process never answered a ping. The log it names holds `vmlabd`'s stdout and stderr. Common causes: a stale socket from a previous user session, a runtime directory owned by another user, a `vmlab-host.wcl` that fails to parse, and a fast-path probe that crashed rather than degraded. The per-lab daemon's log is `.vmlab/lab.log` inside the lab directory, and `vmlab logs` reads it.

Related messages from the same code path are `spawning vmlabd`, when the binary itself cannot be executed, and `opening {log_path}`, when the state directory is not writable.

## The daemon is another build of vmlab

```
the running vmlabd is vmlab build 0.30.0-alpha, but this vmlab is build 0.31.0+1a2b3c4 — one build cannot drive another's daemons. Run `vmlab daemon stop` (which stops every lab daemon and the machines they run; disks and snapshots stay), then this command again
```

The supervisor and lab daemons outlive the `vmlab` that started them, so a vmlab
installed (or built) while they run meets daemons of the previous build. Every verb
that connects asks the daemon's build (`version`) first and refuses one that differs
from its own; the wire has no protocol version to judge compatibility by, so the
build is the unit. A build is the crate version plus the git revision
(`0.31.0+1a2b3c4`), so two source builds of one version are told apart. The variant
naming `lab "{name}"'s daemon` met a lab daemon; `an older vmlab build, from before
daemons reported their build` met exactly that. Fix: `vmlab daemon stop` (stops
every lab daemon and its machines; disks and snapshots stay), then retry.
`vmlab daemon status` and `vmlab lab list` still answer against old daemons.

## The lab name is already registered

```
lab `{name}` is already registered from {root} — run `vmlab down` there (or `vmlab lab stop {name}` from anywhere) to release it, or run this directory as its own instance with `--instance <id>`
```

The name a lab runs under is its host-global runtime identity, not its directory (ADR-0011). The supervisor compares the requested lab root with the registered one before it hands out a socket or starts a daemon, and a different root is a `conflict` in every registry state, so the command exits 5. This is what two clones of the same repository hit when both declare `lab "demo"` and neither runs as an instance. Two linked git worktrees do not hit it, because each runs under `<declared>-<worktree>`, unless one passes `--instance ""` while the other runs the declared name.

`status`, `down`, `destroy`, `console` and `dev sync` refuse the same way, so the second directory can neither read nor stop the first one's lab.

Release the name with a full `vmlab down` (or `vmlab destroy`) in the directory the message names, or with `vmlab lab stop <name>` from any directory — also the way out when that directory no longer exists. Both stop the machines, keep the clones and reap the daemon. Or run this directory's lab as an instance, with `--instance <ID>` or `VMLAB_INSTANCE`, so it registers under a name of its own. Or change the `lab` name in one of the two files. To carry the provisioned machines across, run `vmlab lab move <name>` in the directory that should own the lab; the lab arrives under the name that directory runs it as, so the main worktree's `demo` moved into linked worktree `feat-x` becomes `demo-feat-x`.

After upgrading to a vmlab with instances, a lab already running from a linked worktree under its declared name keeps that name, while commands in the worktree resolve to `<declared>-<worktree>` and report it as not running. Release it with `vmlab lab stop <declared>`, or pass `--instance ""` to address it as before.

## The eBPF fast path is unavailable

```
network fast path: userspace (mode auto)
  afxdp unavailable: creating a tap needs CAP_NET_ADMIN. In a container, add `--cap-add NET_ADMIN` (the fast path also needs `--cap-add BPF`)
  sockmap unavailable: not used in auto mode: af_unix kernel splicing measures slower than the userspace fabric (psock backlog workqueue); force with `fastpath = "sockmap"` to evaluate it
```

`vmlab fastpath` asks the supervisor which tier the fabric selected and why each higher tier was skipped. Nothing inspects kernel versions or capability bits: each tier is proved end to end over throwaway sockets at daemon start, and a tier that fails its probe is logged as `fast-path tier {tier} unavailable: {reason}` and skipped. A forced tier that fails degrades to `userspace` rather than stopping the daemon. The lab runs unchanged on the userspace fabric in every case, so none of these reasons is an error.

| Reason | Fix |
| --- | --- |
| `… — run the vmlab daemons with CAP_BPF + CAP_NET_ADMIN to enable` | Grant both capabilities to the daemon binaries, or run them where they hold them. |
| ``the tun driver is not loaded. Run `modprobe tun` on the host (not inside the container) and restart vmlab`` | As it says. If the kernel was upgraded in place, the longer variant of this message tells you to reboot first. |
| `/dev/net/tun is missing. In a container, pass `--device /dev/net/tun`; …` | Expose the device to the container that runs the daemons. |
| ``ignoring VMLAB_FASTPATH=`{value}` (want auto\|off\|sockmap\|afxdp)`` | The environment override is misspelled. The `fastpath` field of the host configuration takes the same four values. |

The `sockmap` tier is never chosen in `auto` mode because it measures slower than the userspace fabric; forcing it is for evaluation only, and a runtime failure there falls back with `sockmap offload unavailable ({error}); using userspace switching`.
