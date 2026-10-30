"""Every user-facing vmlab feature the end-to-end run accounts for.

A scenario reports each one as passed, failed or skipped with a reason.
Anything left unreported is a gap in the suite and fails the run.
"""

FEATURES: dict[str, str] = {
    # -- templates ---------------------------------------------------------
    "template.build.qcow2": "build a template from a cloud qcow2 source",
    "template.build.layered": "build a template layered on another (`source \"template\"`)",
    "template.list": "`vmlab template list`",
    "template.export": "`vmlab template export`",
    "template.import": "`vmlab template import`",
    "template.rm": "`vmlab template rm`",
    "template.clean": "`vmlab template clean` (dry run and apply)",
    "template.registry.login": "`vmlab template login` against a registry",
    "template.registry.push": "`vmlab template push` to an OCI registry",
    "template.registry.pull": "`vmlab template pull` from an OCI registry",
    "template.registry.search": "`vmlab template search --registry`",
    "template.registry.search-latest": "`vmlab template search` shows the version `latest` names over a higher stale tag",
    "template.registry.list-remote": "`vmlab template list --remote`",
    "template.registry.config": "`vmlab template registry add/list/remove`",
    "template.registry.lab-ref": "a lab VM referencing a registry template pulls it on `up`",
    "template.event": "the `template.built` event",
    "template.variant.build": "build a template definition with variants: the base, then each variant on it",
    "template.variant.select": "lab VMs select the default variant, a named `#variant`, and are refused `#base`",
    "template.variant.rebuild": "`vmlab template build <name> --variant <v>` rebuilds one variant, leaving the others",
    # -- lab lifecycle -----------------------------------------------------
    "lab.validate": "`vmlab validate` accepts a good lab",
    "lab.validate.reject": "`vmlab validate` rejects a bad lab with a named error",
    "lab.up": "`vmlab up` brings the lab up",
    "lab.up.partial": "`vmlab up <machine>` brings up one machine",
    "lab.depends_on": "`depends_on` orders machine start",
    "lab.depends_on_provision": "`depends_on_provision` holds a machine's provisions until another's have finished",
    "lab.provision.parallel": "machines nothing orders provision in parallel, each line led by the machine's name",
    "lab.status": "`vmlab status` and `status -v`",
    "lab.down": "`vmlab down`",
    "lab.up.reload": "`vmlab up` after `down` runs the edited lab file; an edit over a running machine leaves it pending until `vm restart`",
    "lab.reload.stopped": "an edit to a stopped machine while others run is reloaded, and `up` boots it on the new declaration",
    "lab.reload.network": "a segment change while the lab runs is refused, naming the segment",
    "lab.up.template-changed": "`vmlab up` refuses a VM whose template changed while its clone is kept",
    "lab.destroy": "`vmlab destroy`",
    "lab.pull": "`vmlab pull` fetches registry templates and images only",
    "lab.list": "`vmlab lab list`",
    "lab.info": "`vmlab lab info`",
    "lab.stop": "`vmlab lab stop`",
    "lab.down.release": "a full `vmlab down` releases the lab name, so another checkout declaring it can `up`",
    "lab.move": "`vmlab lab move` hands a stopped lab's clones to another checkout declaring its name, and refuses while it runs",
    "lab.status.conflict": "`vmlab status` (and `down`) in a checkout whose lab name is registered from another root exit 5 naming it",
    "lab.restart": "`vmlab lab restart` (and its refusal while running)",
    "lab.destroy.named": "`vmlab lab destroy <lab>`",
    "daemon.lifecycle": "`vmlab daemon start/status/stop`",
    # -- vm power and hardware ---------------------------------------------
    "vm.start": "`vmlab vm start`",
    "vm.stop": "`vmlab vm stop` and `--force`",
    "vm.restart": "`vmlab vm restart`",
    "vm.destroy": "`vmlab vm destroy`",
    "vm.ip": "`vmlab vm ip` and `--nic`",
    "vm.sleep": "a guest that suspends to RAM reports `suspended`, agent verbs refuse by name, `vm start` wakes it",
    "vm.prevent_sleep": "`prevent_sleep = true` wakes the guest the moment it suspends, recorded as `vm.woken`",
    "vm.hw.cpus-memory": "declared cpus and memory reach the guest",
    "vm.hw.disk": "an extra `disk` block with a size",
    "vm.hw.disk-from": "a `disk` built from a folder (FAT)",
    "vm.hw.cdrom": "a `cdrom` attachment",
    "vm.hw.floppy": "a `floppy` attachment",
    "vm.hw.tpm": "`tpm = true` (swtpm)",
    "vm.hw.firmware": "`firmware` selection (OVMF)",
    "vm.hw.secure_boot": "`secure_boot = true`",
    "vm.hw.qemu_args": "`qemu_args` passthrough",
    "vm.hw.nested": "`nested = true`",
    "vm.hw.gpu": "`gpu {}` passthrough / virgl",
    # -- host USB passthrough (§5.2.1) ---------------------------------------
    "usb.list": "`vmlab usb list` (and `--json`) lists the host's devices, or says it has none",
    "usb.controller": "`usb = true` boots the VM with an xHCI controller; a VM without it gets none",
    "usb.refusals": "`vmlab usb attach` refuses a VM without `usb = true`, a container, a bad selector and an unknown device, by name",
    "usb.attach": "`vmlab usb attach`/`detach` pass a host device through and back",
    "vm.scratch": "a `template = \"scratch\"` VM",
    "vm.media.iso": "`media` built from a folder as an ISO",
    "vm.media.floppy": "`media` built from a folder as a floppy",
    # -- guest agent -------------------------------------------------------
    "agent.exec": "`vmlab exec` with exit code and output",
    "agent.exec.timeout": "`vmlab exec --timeout`",
    "agent.shell": "`vmlab shell` over the agent (pty)",
    "agent.cp.push": "`vmlab cp` host to guest",
    "agent.cp.pull": "`vmlab cp` guest to host",
    "agent.tail": "`vmlab tail`",
    "agent.osinfo": "`vmlab osinfo`",
    "agent.stats": "`vmlab machine stats`",
    "agent.capabilities": "`vmlab machine capabilities`",
    "agent.clipboard": "`vmlab clipboard set/get`",
    "agent.rehandshake": "an agent restarted inside the guest is handshaken again: status names the new one",
    "agent.update": "`vm start` refreshes a stale guest agent before reporting ready, marks the machine diverged, and status names the running agent",
    "agent.repair": "`vmlab machine repair-agent` and the diverged mark",
    "agent.eventlog": "`vmlab eventlog` (Windows event log)",
    # -- logins ------------------------------------------------------------
    "login.default": "`exec` runs as the declared default login",
    "login.user": "`exec --user <label>` and `--user root`",
    "login.password": "`exec --user --password` for an undeclared account",
    "login.elevated": "`elevated` Windows logins",
    # -- display and vision ------------------------------------------------
    "vision.screenshot": "`vmlab vm screenshot`",
    "vision.ocr": "`vmlab vm ocr` and `--region`",
    "vision.find-image": "`vmlab vm find-image`",
    "vision.sendkeys": "`vmlab vm sendkeys`",
    "vision.mouse": "`vmlab vm mouse-move/click/drag`",
    "console.tcp": "`vmlab console --tcp` serves VNC",
    "console.viewer": "`vmlab console` launching a GUI viewer, `gui = true`",
    # -- console recording (§11.1) ------------------------------------------
    "record.status": "`vmlab record status` and the RECORDING section of `vmlab status`",
    "record.up-from-boot": "`vmlab up --record` captures from the firmware's first frame",
    "record.probe": "ffprobe reads h264, 1920x1080 and a duration near wall time",
    "record.timeline": "`--timeline` lists the inputs sent, in video time",
    "record.ocr": "typed text is legible in a frame after typing",
    "record.ripple": "a click ripple at the click point",
    "record.compress-idle": "`--compress-idle` shortens an idle wait",
    "record.webm": "a .webm path yields VP9 at `--size`",
    "record.refusals": "conflict, no display and .gif refusals",
    "record.restart": "`vm restart` mid-recording yields one continuous file",
    "record.script": "`vmlab script --record` finalises on a failing script",
    "record.wscript": "`m.record`, `Recording.stop()`, and a handle left open",
    # -- networking --------------------------------------------------------
    "net.dhcp": "DHCP leases on a segment",
    "net.static-ip": "a static `nic { ip }`",
    "net.dns.records": "DNS names for machines and `record` blocks",
    "net.dns.sinkhole": "`sinkhole` blocks",
    "net.dns.verb": "`vmlab dns`",
    "net.nat": "NAT egress from a segment",
    "net.nat.bulk": "a long NAT download keeps its throughput while the guest's receive window closes and reopens",
    "net.isolated": "`nic { isolated = true }`",
    "net.forward": "segment `forward` to a machine port",
    "net.forward.bind": "a forward declaring `bind = \"127.0.0.1\"` answers on loopback, is refused on the host's own address, and `status --json` names the address",
    "net.forward.provisioned": "segment `forward` to a machine `up` provisions, installed and logged as `forward.installed`",
    "port.verb": "`vmlab port` prints a forward's bound host port (and `--json` its status entry) as `status --json` reports it, and fails naming the installed forwards",
    "net.routes": "daemon routing between `routes_to` segments (source kept, rules applied, the rest isolated) and `route` blocks",
    "net.l3.block": "`block` rules",
    "net.l3.redirect": "`redirect` rules",
    "net.mtu": "segment `mtu`",
    "net.global": "`global = true` segments shared between labs",
    "net.peer": "cross-host `connect {}` trunks between supervisors",
    "net.fastpath": "`vmlab fastpath` reports the selected tier",
    "net.fastpath.kernel": "the afxdp / sockmap kernel tiers",
    # -- shares ------------------------------------------------------------
    "share.virtiofs": "`share` over virtiofs",
    "share.smb": "`share` over SMB",
    "share.readonly": "`share { readonly = true }`",
    "share.smb1": "SMB1 shares for legacy guests",
    "share.unmountable": "`vmlab up` warns, before saying the lab is up, about a share that will not mount",
    "share.virtiofs.old-virtiofsd": "a virtiofs share on a virtiofsd without `--migration-mode` (Ubuntu 24.04's 1.10.0)",
    # -- containers --------------------------------------------------------
    "container.up": "an OCI container runs as a micro-VM",
    "container.idle": "`mode = :idle`",
    "container.env": "container `env`",
    "container.volume": "container `volume` (host path and named)",
    "container.port": "container `port` published on the host",
    "container.healthcheck": "container `healthcheck` and `container.unhealthy`",
    "container.exec": "`vmlab container exec`",
    "container.logs": "`vmlab container logs`",
    "container.shell": "`vmlab container shell` (pty)",
    "container.power": "`vmlab container start/stop/restart/ip/destroy`",
    # -- snapshots ---------------------------------------------------------
    "snapshot.vm.online": "online snapshot of a running VM, create and restore",
    "snapshot.vm.offline": "offline snapshot of a stopped VM",
    "snapshot.lab": "`vmlab snapshot create` for the whole lab",
    "snapshot.list-delete": "`vmlab snapshot list` and `delete`",
    "snapshot.container": "snapshots of a container",
    "snapshot.clock.set": "an online restore sets the guest clock to the host's time (VM and container)",
    "snapshot.clock.kept": "`clock = \"snapshot\"` keeps the time a restored guest was captured at",
    "snapshot.clock.fixed": "a fixed `clock` starts the guest at the declared date",
    "snapshot.virtiofs.old-virtiofsd": "online snapshots refused, offline ones taken, on a virtiofsd too old to migrate",
    # -- worktree labs (§21) -----------------------------------------------
    "worktree.isolation.status": "`vmlab status` (and `--json`) in a frozen family says the main and the worktree lab run under worktree isolation",
    "worktree.isolation.forward": "under worktree isolation a forward binds 127.0.0.1: reachable on loopback, not on the host's address",
    "worktree.isolation.nat": "a worktree guest is refused (RST) the main lab's forward and any other host address through NAT",
    "worktree.isolation.global": "a worktree lab joins its global segment as `<segment>@<worktree>`, with its own DNS, the main lab keeping the plain name",
    "worktree.ports.shift": "a fixed `host_port` binds exactly in the main lab and one up in the first worktree lab, both answering on 127.0.0.1; `up` says so and `status` shows the declared port",
    "worktree.ports.stable": "a worktree lab keeps its shifted port across `down` and `up`",
    # -- scripting and events ----------------------------------------------
    "script.run": "`vmlab script` runs a wscript against the lab",
    "script.provision": "`provision` scripts run on `up`",
    "script.as_login": "wscript `as_login` / `as_account`",
    "script.segment-api": "wscript segment API (dns_set, block, forward, route_to/unroute_to)",
    "script.vision-api": "wscript screenshot / OCR / wait_for_text",
    "script.terminal": "wscript `terminal()` send / expect",
    "script.snapshot-api": "wscript snapshot / restore",
    "events.handlers": "`on` handlers fire for lab and machine events",
    "logs.streams": "`vmlab logs` events, serial, qemu and console streams",
    "logs.follow": "`vmlab logs -f`",
    # -- playbooks ---------------------------------------------------------
    "playbook.up": "a `playbook` block applied on `up`",
    "playbook.list": "`vmlab playbook list`",
    "playbook.check-apply": "`vmlab playbook check` and `apply`",
    "playbook.timeout": "a run past its `timeout` fails, and the next run is refused until what it started has finished",
    # -- dev machines ------------------------------------------------------
    "dev.sync.host-to-guest": "workspace host edits reach the guest",
    "dev.sync.guest-to-host": "workspace guest edits reach the host",
    "dev.sync.modes": "executable bits travel both ways",
    "dev.sync.status-flush": "`vmlab dev sync status` and `flush`",
    "dev.sync.conflict": "a conflict halts, `dev sync diff` shows it, `resolve` clears it",
    "dev.select": "`dev sync` machine selection (argument, VMLAB_DEV_MACHINE)",
    "dev.snapshot-bracket": "snapshot capture flushes; restore re-seeds and `--discard-guest-changes`",
    # -- shared apps -------------------------------------------------------
    "app.x11": "`vmlab app run` on an X11-only host picks X11 and shows the guest window there",
    "app.interrupt": "Ctrl-C on `vmlab app run` exits 130 and kills the guest program",
    "app.exit-code": "`vmlab app run` exits with the guest program's code",
    "app.waypipe": "`vmlab app run` over waypipe on a Wayland host",
    "app.rdp": "`vmlab app run` over RDP RemoteApp from a Windows guest",
    # -- worktree labs -----------------------------------------------------
    "worktree.clone": "`up` in a linked worktree clones each machine from the main checkout's freeze",
    "worktree.clone.macs": "a worktree machine keeps the frozen MACs, and the main and worktree labs run at once",
    # -- architectures -----------------------------------------------------
    "arch.emulated": "a non-x86 guest under TCG emulation",
}

# -- hypervisor scope (PRD §22) --------------------------------------------
#
# Each feature carries the vmlab-hv phase that makes it applicable: a run on
# `vmlab` reports a feature past the phases delivered as skipped, whatever its
# scenario did, because a lab mixing kinds still runs its VMs on QEMU and a
# pass there says nothing about vmlab-hv. `host` marks a feature no hypervisor
# takes part in (the template store, validation, the supervisor); it runs on
# both. Phase 2's sub-phases (2a platform and SeaBIOS, 2b frames, 2c OVMF, SMM,
# TPM and Windows, 2d GPU, 2e virtio-fs and audio, 2f USB and vfio) land in
# order, and the suite's one VM template boots OVMF, so no VM feature here is
# applicable before 2c. A feature that touches a VM and a container is tagged
# by the VM.

HOST = "host"

# Every phase in the order vmlab-hv delivers them.
PHASE_ORDER: tuple[str, ...] = (HOST, "1", "2a", "2b", "2c", "2d", "2e", "2f", "3", "4")

# The phases vmlab-hv has delivered; `run.py --vmlab-phase` overrides it.
VMLAB_DELIVERED = "1"

_GROUPS: dict[str, list[str]] = {
    HOST: [
        "template.list", "template.export", "template.import", "template.rm", "template.clean",
        "template.registry.login", "template.registry.push", "template.registry.pull",
        "template.registry.search", "template.registry.search-latest", "template.registry.list-remote",
        "template.registry.config", "template.event", "lab.pull",
        "lab.validate", "lab.validate.reject", "daemon.lifecycle", "usb.list", "net.fastpath",
    ],
    # Container micro-VMs.
    "1": [
        "container.up", "container.idle", "container.env", "container.volume", "container.port",
        "container.healthcheck", "container.exec", "container.logs", "container.shell", "container.power",
        "snapshot.container", "snapshot.clock.kept", "snapshot.clock.fixed",
        "dev.sync.host-to-guest", "dev.sync.guest-to-host", "dev.sync.modes", "dev.sync.status-flush",
        "dev.sync.conflict", "dev.select", "dev.snapshot-bracket",
        "app.x11", "app.interrupt", "app.exit-code", "app.waypipe",
    ],
    # Modern x86_64 VMs, on the e2e template's OVMF.
    "2c": [
        "template.build.qcow2", "template.build.layered", "template.registry.lab-ref",
        "template.variant.build", "template.variant.select", "template.variant.rebuild",
        "lab.up", "lab.up.partial", "lab.depends_on", "lab.depends_on_provision", "lab.provision.parallel",
        "lab.status", "lab.down", "lab.up.reload", "lab.reload.stopped", "lab.reload.network",
        "lab.up.template-changed", "lab.destroy", "lab.list", "lab.info", "lab.stop", "lab.down.release",
        "lab.move", "lab.status.conflict", "lab.restart", "lab.destroy.named",
        "vm.start", "vm.stop", "vm.restart", "vm.destroy", "vm.ip", "vm.sleep", "vm.prevent_sleep",
        "vm.hw.cpus-memory", "vm.hw.disk", "vm.hw.disk-from", "vm.hw.cdrom", "vm.hw.tpm",
        "vm.hw.firmware", "vm.hw.secure_boot", "vm.hw.nested", "vm.scratch", "vm.media.iso",
        "agent.exec", "agent.exec.timeout", "agent.shell", "agent.cp.push", "agent.cp.pull", "agent.tail",
        "agent.osinfo", "agent.stats", "agent.capabilities", "agent.clipboard", "agent.rehandshake",
        "agent.update", "agent.repair", "agent.eventlog",
        "login.default", "login.user", "login.password", "login.elevated",
        "vision.screenshot", "vision.ocr", "vision.find-image", "vision.sendkeys", "vision.mouse",
        "console.tcp", "console.viewer",
        "record.status", "record.up-from-boot", "record.probe", "record.timeline", "record.ocr",
        "record.ripple", "record.compress-idle", "record.webm", "record.refusals", "record.restart",
        "record.script", "record.wscript",
        "net.dhcp", "net.static-ip", "net.dns.records", "net.dns.sinkhole", "net.dns.verb", "net.nat",
        "net.nat.bulk", "net.isolated", "net.forward", "net.forward.bind", "net.forward.provisioned",
        "port.verb", "net.routes", "net.l3.block", "net.l3.redirect", "net.mtu", "net.global", "net.peer",
        "net.fastpath.kernel", "share.smb",
        "snapshot.vm.online", "snapshot.vm.offline", "snapshot.lab", "snapshot.list-delete",
        "snapshot.clock.set",
        "worktree.isolation.status", "worktree.isolation.forward", "worktree.isolation.nat",
        "worktree.isolation.global", "worktree.ports.shift", "worktree.ports.stable",
        "worktree.clone", "worktree.clone.macs",
        "script.run", "script.provision", "script.as_login", "script.segment-api", "script.vision-api",
        "script.terminal", "script.snapshot-api", "events.handlers", "logs.streams", "logs.follow",
        "playbook.up", "playbook.list", "playbook.check-apply", "playbook.timeout",
        "app.rdp",
    ],
    "2d": ["vm.hw.gpu"],
    "2e": ["share.virtiofs", "share.readonly", "share.unmountable"],
    "2f": ["usb.controller", "usb.refusals", "usb.attach"],
    # TCG emulation.
    "3": ["arch.emulated"],
    # The legacy PC: floppy controller, SMB1 guests.
    "4": ["vm.hw.floppy", "vm.media.floppy", "share.smb1"],
}

# Features only QEMU can ever run, and why; skipped on every other hypervisor.
QEMU_ONLY: dict[str, str] = {
    "vm.hw.qemu_args": "`qemu_args` is QEMU's command line; vmlab-hv refuses it",
    "share.virtiofs.old-virtiofsd": "tests QEMU's external virtiofsd; vmlab-hv serves virtio-fs in process",
    "snapshot.virtiofs.old-virtiofsd": "tests QEMU's external virtiofsd; vmlab-hv serves virtio-fs in process",
}

PHASES: dict[str, str] = {fid: phase for phase, ids in _GROUPS.items() for fid in ids}

_untagged = set(FEATURES) - set(PHASES) - set(QEMU_ONLY)
_unknown = (set(PHASES) | set(QEMU_ONLY)) - set(FEATURES)
_twice = sum(len(ids) for ids in _GROUPS.values()) + len(QEMU_ONLY) - len(set(PHASES) | set(QEMU_ONLY))
assert not _untagged and not _unknown and not _twice, (
    f"phase tags out of step with FEATURES: untagged {sorted(_untagged)}, unknown {sorted(_unknown)}, "
    f"{_twice} tagged twice"
)


def out_of_scope(feature: str, hypervisor: str, delivered: str = VMLAB_DELIVERED) -> str | None:
    """Why `feature` does not apply on `hypervisor` with phases through
    `delivered` landed, or None when it does."""
    if hypervisor == "qemu":
        return None
    if feature in QEMU_ONLY:
        return f"QEMU only: {QEMU_ONLY[feature]}"
    phase = PHASES[feature]
    if PHASE_ORDER.index(phase) > PHASE_ORDER.index(delivered):
        return f"vmlab-hv phase {phase}"
    return None
