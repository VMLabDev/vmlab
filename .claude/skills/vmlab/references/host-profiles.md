# Host configuration, guest OS profiles, and `vmlab osinfo`

Per-host settings live in one optional host configuration file; per-guest-OS
hardware defaults live in guest OS profiles. Everything else is in the lab file
(see lab-file.md).

## The host config file

vmlab reads `config.wcl` from its XDG config directory: `~/.config/vmlab/config.wcl`,
honouring `XDG_CONFIG_HOME`.

- The file is optional. Absent file means every default applies.
- Every field is an override; an absent field leaves the default in place.
- It must start with `import <vmlab-host.wcl>` and contain one `host { … }` block.
  A file without the import is rejected with an error naming the missing line.
- A malformed value is reported at its line, with the same wording a lab file gets.
  Every mistake in the file is reported in one pass.
- Errors surface where a lab is loaded, so a broken file fails `vmlab up` with its
  lines named. The daemon processes fall back to defaults for a file they cannot read.
- The daemons read the file when they start, so a change takes effect on the next
  `vmlab up` after the supervisor restarts.

```wcl
# ~/.config/vmlab/config.wcl
import <vmlab-host.wcl>

host {
  subnet_pool        = "10.99.0.0/16"
  dns_suffix         = "lab.local"
  disk_low_percent   = 5
  viewer             = "remote-viewer vnc://{}"
  oci_chunk_size     = 128MiB
  workspace_max_file = 2GiB
}
```

```wcl
# ~/.config/vmlab/config.wcl
import <vmlab-host.wcl>

host {
  subnet_pool = "10.99.0.0/16"
  dns_suffix  = "lab.local"
  psk         = "shared-secret"
  viewer      = "vncviewer {}"
}
```

### `host {}`

The one block the file carries.

```wcl
host {
  subnet_pool          = "10.213.0.0/16"
  dns_suffix           = "vmlab.internal"
  dns_upstream         = "1.1.1.1:53"
  disk_low_percent     = 10
  psk                  = "…"
  trunk_port           = 13947
  viewer               = "vncviewer {}"
  fastpath             = "auto"
  oci_chunk_size       = 512MiB
  config_weave_bin_dir = "~/.local/share/config-weave/bin"
  ssh_config           = "~/.ssh/config"
  workspace_max_file   = 256MiB
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `subnet_pool` | utf8 | `10.213.0.0/16` | CIDR the automatic /24 segment subnets are carved from; segments with no `subnet` are auto-allocated one /24 each (see networking.md). |
| `dns_suffix` | utf8 | `vmlab.internal` | Suffix for auto-registered machine names in segment DNS, `<vm>.<lab>.<suffix>`. |
| `dns_upstream` | utf8 | host resolver | Upstream resolver as `ip[:port]` for queries the lab DNS cannot answer. |
| `disk_low_percent` | i64 | `10` | Free-space percentage, 0 to 100, below which the `host.disk_low` watchdog fires (see automation.md). |
| `psk` | utf8 | none | Pre-shared key authenticating cross-host segment trunks. Set the same value on both hosts. |
| `trunk_port` | i64 | `13947` | TCP port the supervisor listens on for inbound cross-host segment trunks, 1 to 65535. |
| `viewer` | utf8 | none (auto-detected) | VNC viewer command; `{}` is replaced by the target. |
| `fastpath` | utf8 | `auto` | Network fast path: `auto` probes, `off` forces userspace, `sockmap` and `afxdp` force a kernel tier. |
| `oci_chunk_size` | ByteSize | `512MiB` | OCI layer chunk size for `vmlab template push` (see templates.md). |
| `config_weave_bin_dir` | utf8 | `~/.local/share/config-weave/bin` | Directory holding the config-weave guest binaries playbooks push (see automation.md). |
| `ssh_config` | utf8 | `~/.ssh/config` | File vmlab writes its managed SSH block into (see logins-and-ssh.md). |
| `workspace_max_file` | ByteSize | `256MiB` | Workspace syncer per-file size guard. A larger file is refused by name (see dev-machines.md). |

Parser rules, all violations reported in one pass:

- `subnet_pool` is a well-formed CIDR.
- `disk_low_percent` is between 0 and 100.
- `trunk_port` is between 1 and 65535.
- `fastpath` is one of `auto`, `off`, `sockmap`, `afxdp`. The `VMLAB_FASTPATH`
  environment variable selects the same values.
- `oci_chunk_size` and `workspace_max_file` are non-negative sizes.
- A field the schema does not name is rejected with its position.

Two fields are location knobs with one code path behind them rather than switches:

- `ssh_config` moves the managed SSH block. The block is written to the named file
  and the `ssh -G` check still runs against it, so a block redirected somewhere
  OpenSSH does not read warns honestly rather than pretending to work.
- `config_weave_bin_dir` is the first rung of a three-rung lookup, ahead of the
  `VMLAB_CONFIG_WEAVE_DIR` environment variable and the XDG default.

`workspace_max_file` is host config rather than a `@dev` argument because the cap
is about this developer's link to the guest, not the lab everyone shares; the
refusal message names the field.

Fast path note: `auto` never selects `sockmap`; it was measured slower than the
userspace fabric and exists for explicit evaluation. Both kernel tiers need
CAP_BPF and CAP_NET_ADMIN, and a daemon that cannot prove a tier works on its host
falls back to userspace silently. `vmlab fastpath` reports the tier in use.

### The viewer

`vmlab console` and `gui = true` launch a viewer chosen the same way:

- An explicit `viewer` in the host config wins and is dialled at the VNC unix
  socket directly.
- Otherwise vmlab takes the first of `remote-viewer`, `gvncviewer` and `vncviewer`
  found on `PATH`; all three are driven over a localhost TCP bridge to the socket,
  held open by a detached helper that exits when the viewer window closes, so
  neither command ties up the terminal.
- With no viewer at all, or with `--tcp`, `vmlab console` bridges the socket to a
  localhost port and prints the address to point any VNC client at.

Closing a viewer only disconnects; the VM keeps running, always headless behind VNC.

## Directories

vmlab follows the XDG layout and honours each variable that overrides it.

| Path | Holds |
| --- | --- |
| `~/.config/vmlab/` | `config.wcl` and the `profiles/` directory of user profiles. |
| `~/.local/share/vmlab/` | The template store under `templates/` and the container image cache under `oci/`. |
| `~/.local/share/config-weave/bin/` | Where config-weave's own install puts its guest binaries. |
| `~/.local/state/vmlab/` | Daemon state, per-lab logs and event history. |
| `$XDG_RUNTIME_DIR/vmlab/` | Control sockets: the supervisor's, each lab's, and per-VM QMP, agent and VNC sockets, plus the SSH multiplexer sockets under `ssh/`. |
| `<lab>/.vmlab/` | The lab's own working data: disk clones, built media, TPM state, persisted state, sync ledgers and the `dev use` selection. Gitignore it. |

The runtime directory is a full-privilege interface, since a client that can connect
to a lab socket runs scripts in the lab and reads and writes guest files. vmlab
creates it `0700`, tightens an existing one, and refuses one owned by someone else.

`VMLAB_WORK_DIR` relocates every lab's `.vmlab/` under one base, namespaced by lab
name and a hash of its root, keeping the write-heavy working data off a slow
filesystem while the lab file stays put.

## WSL 2

vmlab is clean on WSL 2 for the same reasons it needs no privileges elsewhere: the
network fabric is userspace with no tap, bridge or macvlan, and the only kernel
grant KVM needs is `/dev/kvm`. Four things are specific to WSL 2.

- **Nested virtualisation must be enabled** in `.wslconfig` for `/dev/kvm` to exist
  inside the distribution. Without it every VM runs under TCG.
- **`XDG_RUNTIME_DIR` may be missing.** Some WSL setups do not set it; vmlab falls
  back to `/tmp/vmlab-<uid>`, created private and refused if owned by anyone else.
  The SSH `ControlPath` lives under the same directory.
- **The viewer lives on the Windows side.** Run `vmlab console --tcp` to get a
  localhost address and point a Windows VNC client at it; WSL's localhost forwarding
  carries it across. Host access to guest services works the same way, through port
  forwards and localhost forwarding.
- **The disk-space watchdog matters more.** WSL 2's ext4 VHDX grows and does not
  shrink, and linked clones grow with use, so `disk_low_percent` and the
  `host.disk_low` event are worth handling.

## KVM or TCG

For each machine vmlab picks the accelerator once: KVM when `/dev/kvm` can be opened
and the guest architecture is the host's, TCG otherwise. TCG is full emulation, slow
but functional, and vmlab warns loudly when it falls back, naming the machine and the
architecture. Two cases hit it on purpose: a foreign-architecture guest such as an
aarch64 or riscv64 template on an x86_64 host, which can only ever be emulated, and a
host with no `/dev/kvm`. Give an emulated guest a couple of minutes to boot.

Nested virtualisation inside a guest is separate: `nested = true` on a VM passes the
host CPU through, which is what exposes VMX or SVM to the guest.

If every VM is slow and `vmlab logs` shows the TCG fallback warning for x86_64
guests, the host lacks KVM: on WSL 2 enable nested virtualisation in `.wslconfig`,
on a bare host check that your user can open `/dev/kvm` (see troubleshooting.md).

## Guest OS profiles

A profile is a named bundle of known-good hardware defaults for one family of guest
operating system. Profiles are data, shipped as WCL and extensible from a directory
in the config.

### What a profile decides

| Field | Decides |
| --- | --- |
| `machine` | The QEMU machine type: `q35` or `pc` (i440fx). |
| `firmware`, `secure_boot`, `tpm` | OVMF or SeaBIOS, secure boot under OVMF, and an swtpm 2.0 device. |
| `disk_bus`, `nic_model`, `display` | The devices the guest can drive: `virtio`, `ide` or `sata` disks; a NIC model such as `virtio-net-pci`, `e1000` or `pcnet`; a display device such as `virtio-vga`, `std` or `cirrus-vga`. |
| `cpus`, `memory` | The hardware floor a VM or container inherits when neither its block nor its template says. |
| `agent_transport` | The device the guest agent's channel rides: `virtio-serial` (default), `isa-serial` for a guest with no virtio drivers, where the legacy agent speaks over COM1, or `none` for a guest nothing can run an agent on. The older `agent_channel` bool still loads as an alias. |
| `input_transport` | How `send_keys` and the mouse reach the guest: `qmp` (default) or `vnc` (see snapshots-vision.md). |
| `virtiofs` | Whether the guest mounts virtiofs natively, which makes it a candidate for `transport = "auto"` shares (see shares-media.md). |
| `workspace_guest` | The guest path an `@dev` workspace lands at when the decorator names none (see dev-machines.md). |

The profile also classifies the guest as Windows-family or Linux-family, which is
what the `login {}` validation rules and the Windows preconditions of the workspace
syncer key on. A `login {}` block's family rules read the resolved profile's name:
`windows*` is Windows, `linux*` is Linux, and any other name is unknown and gets
neither rule.

A profile does not install the agent; that happens once, at template build, when the
build stages the agent binaries on the bootstrap ISO and the template's
unattended-install hook runs the install script (see templates.md).

### Where profiles live

The shipped profiles are compiled into the binary. User profiles are `*.wcl` files
in the `profiles` directory of vmlab's XDG config directory,
`~/.config/vmlab/profiles/` by default. vmlab loads the shipped set, then every
`*.wcl` file there in sorted filename order.

Each file must start with `import <vmlab-profile.wcl>` and contains
`profile "<name>" { … }` blocks. A file without the import is rejected, and so is any
file with an unknown field, a bad keyword or a wrong type, with the line named.

A user profile whose name matches a shipped one **replaces it entirely**, field by
field from scratch, rather than merging. A file holding only
`profile "windows-11" { machine = "pc" }` produces a `windows-11` with no firmware,
no TPM, no disk bus, no memory and no device choices. Copy the full shipped block and
edit it. A new name extends the set and is usable from any lab's `profile =`.

A profile file is validated with the same wording as a lab file: an unknown field, a
value outside its set such as `machine = "vax"`, a `cpus` below 1 or a missing import
are all reported at their line, and every mistake in a file is reported in one pass.
A lab naming a profile that does not exist fails `vmlab validate`; validation reports
an unknown profile name on the block that names it.

Dropping your own `container` profile into the profiles directory raises the micro-VM
defaults for every lab on this host, which is simpler than adding `memory = …` to
each container block.

```wcl
# ~/.config/vmlab/profiles/mine.wcl
import <vmlab-profile.wcl>

profile "freebsd" {
  description = "FreeBSD: q35, SeaBIOS, virtio disk and NIC"
  machine     = "q35"
  firmware    = "seabios"
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
  workspace_guest = "/usr/src"
}
```

```wcl
# ~/.config/vmlab/profiles/mine.wcl
import <vmlab-profile.wcl>

profile "freebsd" {
  description = "FreeBSD 14: q35, SeaBIOS, virtio disk and NIC"
  machine     = "q35"
  firmware    = "seabios"
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
}
```

### `profile {}`

One profile. Every field is optional. A field left unset means the QEMU default
applies for that device, which is what the shipped `custom` profile relies on.

```wcl
profile "<name>" {
  description     = "…"
  machine         = "q35"
  firmware        = "ovmf"
  secure_boot     = false
  tpm             = false
  disk_bus        = "virtio"
  nic_model       = "virtio-net-pci"
  display         = "virtio-vga"
  cpus            = 2
  memory          = 4GiB
  agent_transport = "virtio-serial"
  input_transport = "qmp"
  virtiofs        = false
  workspace_guest = "/src"
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | Profile name, what a `vm`, `container` or `template` names in `profile =`; the inline block label. |
| `description` | utf8 | none | Human-readable summary. Parsed and kept; no verb surfaces it yet. |
| `machine` | utf8 | QEMU default | Machine type: `q35`, or `pc` for i440fx. |
| `firmware` | utf8 | QEMU default | Firmware: `ovmf` or `seabios`. |
| `secure_boot` | bool | unset | Enable secure boot; OVMF only. |
| `tpm` | bool | unset | Attach a swtpm 2.0 device. |
| `disk_bus` | utf8 | QEMU default | Primary disk bus: `virtio`, `ide` or `sata`. |
| `nic_model` | utf8 | QEMU default | NIC device model, for example `virtio-net-pci`, `e1000`, `rtl8139` or `pcnet`. |
| `display` | utf8 | QEMU default | Display device, for example `virtio-vga`, `qxl`, `std` or `cirrus-vga`. |
| `cpus` | i64 | unset | Default vCPU count, at least 1. |
| `memory` | ByteSize | unset | Default RAM, for example `4GiB`. |
| `agent_transport` | utf8 | `virtio-serial` | The guest agent channel's device: `virtio-serial`, `isa-serial` (a 16550 on COM1 for the legacy agent; the serial log moves to COM2), or `none` (no agent; never ready by handshake). |
| `agent_channel` | bool | unset | Superseded by `agent_transport`; still accepted: `true` reads as `virtio-serial`, `false` as `none`, and `agent_transport` wins when both are present. |
| `input_transport` | utf8 | `qmp` | How scripted input reaches the guest: `qmp` send-key, or `vnc` for guests that ignore the PS/2 path. |
| `virtiofs` | bool | `false` | The guest mounts virtiofs natively, so `transport = "auto"` shares use it instead of SMB. |
| `workspace_guest` | utf8 | unset | Guest path an `@dev` workspace lands at when the decorator names none. |

The parser enforces the keyword sets above for `machine`, `firmware`, `disk_bus` and
`input_transport`, `cpus` at least 1, and a non-negative `memory`. `nic_model` and
`display` are passed to QEMU as written. A profile that sets no `workspace_guest`
still hosts a dev machine; the floor of `/src` applies. On a non-x86 `virt` machine a
`virtio-vga` display is downgraded to `virtio-gpu-pci`, which has no legacy VGA.

### The shipped set

| Profile | Machine | Firmware | Secure boot | TPM | Disk bus | NIC model | Display | CPUs | Memory | Other |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `windows-11` | q35 | ovmf | true | true | virtio | virtio-net-pci | virtio-vga | 4 | 8GiB | |
| `windows-10` | q35 | ovmf | false | false | virtio | virtio-net-pci | virtio-vga | 4 | 8GiB | |
| `windows-server` | q35 | ovmf | false | true | virtio | virtio-net-pci | virtio-vga | 4 | 8GiB | |
| `windows-legacy` | pc | seabios | false | false | ide | e1000 | std | 2 | 2GiB | Vista, 7 and 2008-era guests; virtio-serial agent (virtio-win covers this era). |
| `windows-xp` | pc | seabios | false | false | ide | e1000 | std | 2 | 2GiB | `agent_transport = "isa-serial"`: NT4 through XP/2003, the legacy agent on COM1. |
| `windows-9x` | pc | seabios | false | false | ide | pcnet | cirrus-vga | 1 | 256MiB | `input_transport = "vnc"`, `agent_transport = "isa-serial"`. DOS, Windows 3.x to ME. |
| `templeos` | pc | seabios | false | false | ide | e1000 | std | 2 | 512MiB | `agent_transport = "isa-serial"`. TempleOS: the HolyC agent on COM1; no network by design. |
| `linux-modern` | q35 | ovmf | false | false | virtio | virtio-net-pci | virtio-vga | 2 | 4GiB | `virtiofs = true`. |
| `linux-generic` | q35 | seabios | false | false | virtio | virtio-net-pci | std | 2 | 2GiB | Older or unusual distros. |
| `container` | | | | | | | | 1 | 256MiB | Micro-VM size for an OCI container; nothing else applies to a container. |
| `custom` | | | | | | | | | | Nothing assumed; supply everything on the VM or template and in `qemu_args`. |

A blank cell means the field is unset. Every Windows profile lands an `@dev`
workspace at `C:\src` and every Linux one at `/src`; the `container` profile also
sets `workspace_guest = "/src"`, and `custom` leaves it at the floor.

`linux-modern` requests a VGA-compatible virtio GPU on x86 and downgrades to
`virtio-gpu-pci` automatically on the non-x86 `virt` machine, which has no legacy VGA.

The `container` profile carries only a size because a container micro-VM boots
vmlab's own guest asset directly and has no firmware, disk bus, display or NIC model
to choose. Its floor is an order of magnitude below a VM's on purpose: the micro-VM
runs one process, not an operating system. Raise it per container with `cpus` and
`memory`, or drop your own `container` profile into the user directory to change the
default for every lab.

`custom` sets nothing at all, so QEMU's defaults apply to whatever the VM and
template leave unset.

The shipped file is `src/profiles/shipped.wcl` in the repository:

```wcl
# src/profiles/shipped.wcl
import <vmlab-profile.wcl>

// Shipped guest OS profiles (PRD §5.3). Known-good hardware defaults;
// values not set on the VM or template inherit from these. Override or
// extend by dropping `*.wcl` files into `~/.config/vmlab/profiles/`.

profile "windows-11" {
  description = "Windows 11: q35, OVMF with secure boot, swtpm 2.0, virtio devices"
  machine     = "q35"
  firmware    = "ovmf"
  secure_boot = true
  tpm         = true
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "virtio-vga"
  cpus        = 4
  memory      = 8GiB
  // Where an `@dev` workspace lands when the decorator names no
  // `workspace_guest` (§19.1) — the key is guest-OS-shaped, which is why it
  // is profile-sourced.
  workspace_guest = "C:\\src"
}

profile "windows-10" {
  description = "Windows 10: q35, OVMF, virtio devices (no TPM/secure boot required)"
  machine     = "q35"
  firmware    = "ovmf"
  secure_boot = false
  tpm         = false
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "virtio-vga"
  cpus        = 4
  memory      = 8GiB
  workspace_guest = "C:\\src"
}

profile "windows-server" {
  description = "Windows Server: q35, OVMF, swtpm 2.0, virtio devices"
  machine     = "q35"
  firmware    = "ovmf"
  secure_boot = false
  tpm         = true
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "virtio-vga"
  cpus        = 4
  memory      = 8GiB
  workspace_guest = "C:\\src"
}

profile "windows-legacy" {
  description = "Vista/7/2008-era guests with no virtio storage/net drivers at install time: i440fx, SeaBIOS, IDE disk, e1000 NIC, std VGA; virtio-serial agent channel (virtio-win covers this era)"
  machine     = "pc"
  firmware    = "seabios"
  secure_boot = false
  tpm         = false
  disk_bus    = "ide"
  nic_model   = "e1000"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
  workspace_guest = "C:\\src"
}

profile "windows-xp" {
  description = "NT4 through XP/2003: i440fx, SeaBIOS, IDE disk, e1000 NIC, std VGA; no virtio drivers at all, so the agent channel is a 16550 on COM1 (the legacy agent)"
  machine     = "pc"
  firmware    = "seabios"
  secure_boot = false
  tpm         = false
  disk_bus    = "ide"
  nic_model   = "e1000"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
  agent_transport = "isa-serial"
  workspace_guest = "C:\\src"
}

profile "windows-9x" {
  description = "DOS / Windows 3.x-ME / 2000-era PCs: i440fx, SeaBIOS, IDE disk, Cirrus VGA, AMD PCnet NIC (all drivable by these guests); cap RAM low per template"
  machine     = "pc"
  firmware    = "seabios"
  secure_boot = false
  tpm         = false
  disk_bus    = "ide"
  nic_model   = "pcnet"
  display     = "cirrus-vga"
  cpus        = 1
  memory      = 256MiB
  // Real-mode DOS/9x TUIs (fdisk, setup) drop QMP send-key events between menu
  // redraws; drive their keyboard over VNC instead, which lands reliably.
  input_transport = "vnc"
  workspace_guest = "C:\\src"
}

profile "linux-modern" {
  description = "Modern Linux: q35, OVMF, virtio everything"
  machine     = "q35"
  firmware    = "ovmf"
  secure_boot = false
  tpm         = false
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  // VGA-compatible virtio GPU on x86; auto-downgrades to virtio-gpu-pci on the
  // non-x86 `virt` machine (no legacy VGA there). See display_device_name.
  display     = "virtio-vga"
  cpus        = 2
  memory      = 4GiB
  // Kernel ≥5.4 ships the virtiofs client — `transport = "auto"` shares
  // mount natively instead of over SMB (§7.5).
  virtiofs    = true
  workspace_guest = "/src"
}

profile "linux-generic" {
  description = "Older or unusual distros: q35, SeaBIOS, virtio disk/net, conservative elsewhere"
  machine     = "q35"
  firmware    = "seabios"
  secure_boot = false
  tpm         = false
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
  workspace_guest = "/src"
}

profile "container" {
  description = "Micro-VM defaults for an OCI container (PRD §18): the smallest shape that boots the guest asset and a typical service image"
  // A container micro-VM boots the guest asset directly — no firmware, disk
  // bus, display or NIC model to choose — so this profile carries only the
  // size, which is the whole reason a container names a profile. It is an
  // order of magnitude below the VM floor on purpose: the micro-VM runs one
  // process, not an operating system. Raise it per container with
  // `memory = …`, or drop your own `container` profile into
  // ~/.config/vmlab/profiles to change the default for every lab.
  cpus   = 1
  memory = 256MiB
  workspace_guest = "/src"
}

profile "custom" {
  description = "Nothing assumed — supply everything via VM/template attributes and qemu_args"
}
```

### Where a profile sits in the chain

Hardware resolves **VM block, then template, then profile**. A value set on the
`vm {}` wins; a value not set there comes from the hardware the template recorded
when it was built; a value not set there comes from the profile, and the profile's
defaults are the floor.

The profile a machine resolves against is the one it names, else the one its template
recorded, else the default. The profile is usually inherited from the template rather
than named on the VM, and it is required on a `scratch` VM, which has no template
layer. A template build resolves the build VM's hardware the same way, block over
source template, with the template's own `profile` supplying the floor.

`secure_boot = true` on a VM whose firmware resolves to SeaBIOS is a validation
error, and because either value may have been inherited, the message names the layer
each came from.

Two things a profile supplies resolve through their own chains, not the hardware one:

- A dev machine's `workspace_guest` resolves `@dev` argument, then profile, then the
  `/src` floor. A profile with no dev keys still hosts a dev machine.
- A container's `cpus` and `memory` come from its block or its profile, and one of
  the two must supply each; there is no template layer and no vmlab floor, so a
  container naming no profile must set both.

## `vmlab osinfo`

`vmlab osinfo` prints what a guest's operating system says it is, as reported by the
vmlab agent, as one JSON object. It is written for machine consumption: a tool that
drives vmlab polls it to detect agent readiness and to pick a guest binary.

```sh
vmlab osinfo <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The machine, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

The command asks the machine's agent with a 30-second timeout and prints the answer
on one line. The object has the fields `id`, `name`, `version`, `kernel`, `arch`, and
`hostname`. Unlike most verbs this one always prints JSON; there is no table form and
no `--json` flag.

```sh
$ vmlab osinfo nix01
{"arch":"x86_64","hostname":"nix01","id":"nixos","kernel":"6.12.20","name":"NixOS","version":"25.05"}
```

Exit status is 0 on success. `not_found` (4) means the lab declares no machine by
that name. `failed` (1) covers a machine that is not running and an agent that does
not answer within the timeout, which is what a guest still booting looks like.
