# Glossary

The words vmlab uses, in alphabetical order. Where two words are easy to confuse, such as *login* and *logon* or *snapshot* and *workspace*, the entry says how they differ.

| Term | Meaning |
| --- | --- |
| agent | `vmlab-agent`, the process vmlab runs inside every guest, listening on the `vmlab.agent.0` virtio-serial port. It carries exec, terminals, file transfer, tunnels, the workspace watch, metrics and the clipboard. It is baked into a template at build time and shipped with the host for containers. See architecture.md. |
| attachable | A machine whose agent serves both `tunnel` and `fileops`, and so can carry an editor session over the SSH facade. Reported by `vmlab machine capabilities` and `vmlab status`; a warning at `up`, a refusal at attach. See dev-machines.md. |
| dev machine | A machine carrying the `@dev` decorator: vmlab publishes it as an SSH endpoint an editor attaches into and syncs a workspace onto it. A lab may mark one of them `default = true`. See dev-machines.md. |
| event | A named occurrence in a lab, such as `vm.crashed` or `host.disk_low`, carried on the lab's event stream and written to the event log. See automation.md. |
| fabric | The userspace network vmlab runs every segment on: frame codecs, an L2 switch, DHCP, DNS, a gateway and a NAT engine, all in the lab daemon's process. No tap, bridge or macvlan. See networking.md. |
| facade | The SSH server vmlab terminates on the host for each machine. No guest runs an sshd; the facade answers shells, `sftp` and local forwards over the agent's channels, and refuses what the agent protocol cannot carry. See logins-and-ssh.md. |
| fast path | An optional kernel-assisted tier of the fabric (`afxdp` or `sockmap`) probed at daemon start and used when it works. `vmlab fastpath` says which tier is active. See networking.md. |
| forward | A `forward {}` on a segment, or a `port {}` on a container, that maps a host TCP port onto a guest port. Planned as a whole before any is installed. See networking.md. |
| global segment | A segment with `global = true`, owned by the supervisor rather than one lab, so machines from several labs share it; it can also peer with another host over a trunk. See networking.md. |
| halt | The state a workspace enters when a sync pass finds a path changed on both sides. The whole workspace stops, both directions, on that one machine; nothing is written or deleted. Resolved from the host with the `vmlab dev sync` verbs. See dev-machines.md. |
| handler | An `on "<event>" { run = … }` block in the lab file that runs a wscript when the named event fires. See automation.md. |
| host config | `vmlab-host.wcl`, the per-user file that sets host-wide values: the store location, the viewer command, the fast path mode, the trunk port and PSK. See host-profiles.md. |
| lab | One `lab "<name>" {}` in a `vmlab.wcl`: a set of machines and segments brought up and torn down together. Its name is its host-global identity. See lab-file.md. |
| lab container | A `container {}` in a lab: an OCI image pulled like a docker image and run inside a micro-VM, so it is a lab machine in every respect. See containers.md. |
| lab daemon | The per-lab process the supervisor spawns on `up`: lifecycle, snapshots, the fabric for the lab's segments, events, shares, the syncer and the SSH facade. Logs to `.vmlab/lab.log`. See architecture.md. |
| ledger | The host-side record of what the two sides of a workspace last agreed on, per path. Guest changes are drained into it; the note that a re-seed is owed and the halt a restore refuses on both ride it. See dev-machines.md. |
| linked clone | A VM's disk: a qcow2 whose backing file is the template image in the store. Created on `up` under `.vmlab/`, kept by `down`, deleted by `destroy`. See templates.md. |
| login | A `login {}` block on a machine: a declared guest account a surface attaches as. The SSH user name selects one by label; `vmlab exec --user` does the same. See logins-and-ssh.md. |
| logon | The Windows session vmlab mints for a login: a token from `LogonUser` and a loaded profile, cached per (account, secret, machine). The Linux equivalent is a real session through `su -l` or `setuid`. See logins-and-ssh.md. |
| machine | A VM or a lab container. Every verb that takes a machine name accepts either kind. See architecture.md. |
| marker file | `.vmlab-sync-halt`, the file the syncer writes at the guest's workspace root when the workspace halts, and removes when the halt clears. It is the only signal the guest side receives. See dev-machines.md. |
| media | A `media {}` block: a host folder packed into an ISO or floppy image and attached to a machine, cached by content. See vm.md. |
| micro-VM | The minimal QEMU guest a lab container runs in: vmlab's own kernel and initramfs, with `vmlab-cinit` as PID 1, booting the image's flattened root filesystem. See containers.md. |
| OCI artifact | The form a template takes on a registry: a multi-layer, multi-arch OCI package whose tags are the template's versions. See templates.md. |
| playbook | A `playbook {}` block that runs a config-weave playbook inside the guest, as the machine identity. It has no login rung; anything that must land as a user belongs in a provision. See automation.md. |
| profile | A guest OS profile: WCL data naming the firmware, devices, agent install route and defaults for a family of guests, such as `linux-modern` or `windows-server`. Shipped with vmlab and overridable. See host-profiles.md. |
| provision | A `provision "<script>.ws" {}` block that runs a wscript against a machine once it is ready, in declaration order. The route for anything that must land as a login. See wscript-language.md. |
| prune list | The set of workspace paths the host computes from the layered ignore rules and hands the guest's watch, so the guest never reports what the host would discard. See dev-machines.md. |
| registry | An OCI registry, such as `ghcr.io`, that templates are pushed to and pulled from by ref. A `template` field holding a registry ref is pulled on `up`. See templates.md. |
| re-seed | What follows a snapshot restore on a dev machine instead of a normal sync pass: a host-only, digest-based reconcile that carries the rewound guest back to the canonical copy before the watch reopens, and can emit no guest-to-host action. See snapshots-vision.md. |
| scratch VM | A VM with `template = "scratch"`: a blank qcow2 with no backing image and no template layer in its hardware chain, for installing an OS from nothing. See templates.md. |
| segment | A `segment {}` in a lab: one L2 network with its own subnet, DHCP, DNS and optional NAT egress, routes and forwards. See networking.md. |
| share | A `share {}` on a machine: a host folder mounted in the guest, over virtiofs on Linux or the bundled SMB server on Windows. See shares-media.md. |
| snapshot | A saved state of a machine's disk and memory, captured and restored by `vmlab snapshot`. Not a workspace backup: a dev machine's source lives on the host. See snapshots-vision.md. |
| stat-walk | The syncer's full walk of the guest tree, run only on a watch discontinuity: the first sync, ledger loss, a watch overflow or a dropped channel. An overflow blocks both directions until it completes. See dev-machines.md. |
| store | The per-user directory of installed templates, keyed `arch/name@version`, written only by the supervisor. `vmlab template list` reads it. See templates.md. |
| supervisor | `vmlabd`, one per user, started by the CLI on demand. Spawns and reaps lab daemons, keeps the lab registry, owns global segments and trunks, serialises writes to the store, and runs the host watchdogs. Logs to `vmlabd.log`. See architecture.md. |
| syncer | The workspace syncer: the loop in the lab daemon that keeps a dev machine's workspace and the host tree in step, both ways, as the machine's default login. See dev-machines.md. |
| template | A sealed, read-only disk image in the store that VMs clone from. Built from an ISO or cloud image by a `template {}` file, or pulled from a registry. See templates.md. |
| trunk | The PSK-authenticated TCP link two supervisors bridge a global segment over, so machines on two hosts share one L2 segment. The listen port is `trunk_port` in host config. See networking.md. |
| viewer | The VNC client `vmlab up` opens for a machine with `gui = true` and `vmlab console` opens on demand. Chosen from host config, else found on `PATH`. See snapshots-vision.md. |
| VM | A `vm {}` in a lab: a QEMU guest booted from a linked clone of a template, or from a blank disk when scratch. See vm.md. |
| watchdog | A supervisor check that fires an event on a host condition, such as `host.disk_low` when the filesystem under the store or a lab's clones passes the configured threshold. See automation.md. |
| workspace | The host directory `@dev(workspace = …)` names, mirrored into a dev machine as a guest-local copy of that canonical host tree. The host copy is what survives `destroy`. See dev-machines.md. |
| wscript | The scripting language vmlab's provisions, handlers and `vmlab script` run. vmlab registers its lab, segment and machine API as a wscript host module. See wscript-language.md. |
| wscripti | The `.wscripti` interface file describing that host module, which an editor's wscript language server reads for completion and diagnostics. See wscript-lab-api.md. |
