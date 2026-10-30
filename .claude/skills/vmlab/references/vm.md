# Lab file: `vm {}` and its children

Field-by-field reference for the `vm {}` block and every block it can carry. The
blocks a `vm` shares with a `container` (`login`, `nic`, `provision`,
`playbook`) are documented here once; containers.md refers to them. How a VM's
hardware resolves (VM block > template > profile) is in lab-file.md. Rules the
schema does not carry are enforced by `vmlab validate` and listed under each
entry.

Value types: `utf8` is a quoted string, `bool` is `true` or `false`, `i64` is an
integer, `ByteSize` is an integer with a unit (`8GiB`, `512MiB`), `Duration` is
an integer with a unit (`10s`), `list<utf8>` is a bracketed list of strings.

## vm {}

One virtual machine. The only required field is `template`; every hardware value
not set here is inherited from the template's recorded hardware, then from the
guest OS profile (see host-profiles.md).

```wcl
vm "<name>" {
  template    = "<arch>/<name>[@<version>][#<variant>]"   // or "scratch", or a registry ref
  arch        = "x86_64"
  profile     = "linux-modern"
  cpus        = 2
  memory      = 4GiB
  disk        = 64GiB          // scratch VMs only
  cdrom       = "./isos/boot.iso"
  floppy      = "./boot.img"
  depends_on  = ["dc01"]
  depends_on_provision = ["dc01"]
  nested      = false
  gui         = false
  agent_update = true
  prevent_sleep = false
  clock       = "host"         // or "snapshot", or "2020-01-01T00:00:00Z"
  display     = "virtio-vga"
  cpu_model   = "pentium3"
  hyperv      = true
  accel       = "tcg"
  firmware    = "ovmf"
  tpm         = false
  secure_boot = false
  qemu_args   = []
  gpu       { … }
  nic       { … }
  disk "…"  { … }
  share     { … }
  media     { … }
  login "…" { … }
  provision "…" { … }
  playbook "…" { … }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | VM name, a DNS label, unique per lab; the inline block label. |
| `template` | utf8 | required | `<arch>/<name>[@<version>][#<variant>]` from the store, `scratch`, or an OCI registry reference with an optional `#<variant>`. |
| `arch` | utf8 | from template | Architecture. Required for `scratch` and for registry references. |
| `profile` | utf8 | from template | Guest OS profile supplying hardware defaults. Required for `scratch`. |
| `cpus` | i64 | inherited | vCPU count, greater than 0. Inherited from template, then profile, if omitted. |
| `memory` | ByteSize | inherited | RAM as a byte size, for example `8GiB` or `512MiB`. Inherited if omitted. |
| `disk` | ByteSize | none | Primary disk size, for example `64GiB`. Scratch VMs only; rejected on cloned VMs. |
| `cdrom` | utf8 | none | Path to an ISO to attach as a CD-ROM, relative to the lab root. |
| `floppy` | utf8 | none | Path to a floppy image to attach, relative to the lab root. |
| `depends_on` | list<utf8> | none | Start order: VM or container names that must be ready before this one starts. Ready means a VM's agent answers or a container passes its healthcheck; their provisioning is not waited for. `down` stops in reverse. No self-reference, no cycles. |
| `depends_on_provision` | list<utf8> | none | Provisioning order: VM or container names whose provision and playbook steps must all finish before this machine's first step runs. Does not delay this machine's start. No self-reference, no cycles. |
| `nested` | bool | `false` | Expose hardware virtualisation (VMX/SVM) to the guest. Without it, an x86 guest under KVM has both masked. `up` warns when the host's KVM module has nested virtualisation off. No effect under TCG or on other architectures. |
| `gui` | bool | lab `gui` | Open a VNC viewer on `up`. The VM always runs headless. |
| `agent_update` | bool | lab `agent_update`, else `true` | Refresh the guest agent on `up` and `vm start` when its version stamp differs from the agent this vmlab ships, and mark the VM diverged. `false` leaves the agent the template sealed. Ignored on the legacy agent tier. |
| `prevent_sleep` | bool | `false` | Keep the guest awake: the moment it suspends to RAM (ACPI S3), the lab daemon wakes it with QMP `system_wakeup` and records `vm.woken` with `cause = prevent_sleep`. Declared on the `vm` block only, never inherited: it is the daemon's policy, not hardware. The template build VM always has it on. Not accepted on a `container {}`. Without it a sleeping guest shows `suspended` in `vmlab status` until `vmlab vm start` wakes it. |
| `clock` | utf8 | `host` | Guest clock policy. `host`: the RTC runs from the host's time, the lab daemon checks the guest clock at every agent handshake and sets it when more than 2 s off (`machine.clock_drift`), and after an online snapshot restore it sets the guest's clock to the host's current time through the agent (a failure warns and the restore still succeeds). `snapshot`: a restored guest keeps the time it was captured at. An RFC 3339 date-time such as `2020-01-01T00:00:00Z` (any offset, converted to UTC): the RTC starts at that instant on every boot and runs forward, and vmlab never sets it, after a restore included. The guest's own NTP or w32time can still move it; turn that sync off in the guest, or keep the machine off a segment with a route to a time server, when the date must hold. Declared on the machine block only, never inherited. Also accepted on a `container {}`. |
| `display` | utf8 | inherited | QEMU display device string. Inherited from template, then profile. |
| `cpu_model` | utf8 | inherited | QEMU `-cpu` model, for example `pentium3`, `pentium2`, `qemu32` or `Westmere`, replacing the default (`host` under KVM, `max` under TCG) under either accelerator. For guests the host CPU breaks, such as Windows 9x Setup. Inherited from template, then profile. Any non-empty name is accepted; QEMU refuses an unknown one at start. Cannot be combined with `nested = true`. |
| `hyperv` | bool | inherited | Hyper-V enlightenments for a Windows guest: under KVM on x86 the `-cpu host` string gains `hv-relaxed,hv-vapic,hv-spinlocks=0x1fff,hv-vpindex,hv-runtime,hv-time,hv-synic,hv-stimer,hv-frequencies,hv-reset,hv-tlbflush,hv-ipi`, so Windows reads its clock from the reference TSC page instead of an emulated timer (about 20 ns a read against 5.6 µs native, 51 µs nested under WSL 2). Inherited from template, then profile; the shipped `windows-11`, `windows-10`, `windows-server` and `windows-legacy` profiles set it, so `false` here opts out. Not applied under TCG, with a `cpu_model`, or on a non-x86 guest; `up` warns when this block set `true` and it was not applied. `+invtsc` is never added: it blocks migration, so online snapshots. |
| `accel` | utf8 | inherited | Accelerator: `kvm` or `tcg`. Unset, KVM when usable, else TCG with a warning. `tcg` always emulates, even where KVM works, for guests KVM cannot run reliably, such as Windows 95 on an AMD host; `-cpu` stays `max` unless `cpu_model` names one. `kvm` refuses to start the VM when KVM is unusable rather than falling back. Inherited from template, then profile. `tcg` cannot be combined with `nested = true`. |
| `firmware` | utf8 | inherited | Firmware: `ovmf` or `seabios`. Inherited from template, then profile. |
| `tpm` | bool | inherited | Enable a TPM 2.0 device. Inherited from template, then profile. |
| `usb` | bool | inherited | Boot with a hot-plug xHCI controller (id `usbx`) so `vmlab usb attach` can pass host USB devices in. VM > profile only — a template has no say; off when neither sets it. Changes the VM's hardware: online snapshots taken without it no longer restore, and on q35 the tablet moves to the new controller. On `virt` (aarch64, riscv64), which already has an xHCI, nothing is added: the existing controller is the one devices attach to. |
| `secure_boot` | bool | inherited | Enable secure boot; OVMF only. Inherited from template, then profile. |
| `secure_boot_keys` | utf8 | inherited, else `microsoft` | Key store secure boot starts from: `microsoft` (bundled store, Microsoft's keys) or `setup` (blank store — firmware in setup mode, the guest enrolls its own keys, the VM's `OVMF_VARS.fd` keeps them). Inherited from template, then profile. Needs `secure_boot = true`. |
| `secure_boot_vars` | utf8 | none | Pre-enrolled UEFI VARS file (relative to lab root) copied once at first start instead of the bundled store; must be the secure-boot firmware's VARS size. Needs `secure_boot = true`; exclusive with `secure_boot_keys`; overrides a template's stored VARS. |
| `qemu_args` | list<utf8> | none | Raw QEMU flags appended last. The escape hatch; they win over everything vmlab generates. |
| `gpu {}` | child | none | GPU acceleration: passthrough, virgl or vulkan. |
| `nic {}` | children | none | Network interfaces. No NICs means air-gapped. Shares need at least one. |
| `disk {}` | children | none | Additional disks beyond the primary disk. |
| `share {}` | children | none | Shared folders over virtiofs or SMB. SMB shares require at least one NIC. |
| `media {}` | children | none | ISO or floppy images built from a folder. |
| `login {}` | children | none | Identities `exec`, `shell` and the workspace syncer run as on this VM. Without one every verb keeps the agent identity. |
| `provision {}` | children | none | wscript scripts run on `vmlab up` once this VM is ready, interleaved with its playbooks in declaration order. |
| `playbook {}` | children | none | config-weave playbooks applied on `vmlab up`, interleaved with its provisions in declaration order. |

### Template references

The `template` value takes one of three forms, and the form decides what else
the block must say.

- **Store reference** `<arch>/<name>[@<version>][#<variant>]`, such as
  `x86_64/windows-11@26100.1` or `x86_64/windows-11#win3d` (no `#` = the
  default variant; `#base` is refused for a VM on a template with variants —
  see templates.md). The arch is always written; a bare `windows-11`
  is rejected. Without a version the highest version in the store is used. The
  template must be in the store, and an `arch` field, if present, must match.
- **Registry reference**, such as `ghcr.io/owner/ubuntu-24.04:1.0`. Recognised
  by a first path segment holding a dot, a colon or `localhost`. It requires an
  explicit `arch`. `vmlab up` pulls it if it is absent from the store and never
  re-pulls it implicitly. See templates.md.
- **`scratch`**: no backing image. The VM gets a blank disk and no template
  layer. It requires `arch`, `profile` and `disk`, and boot media is yours to
  attach.

Known architectures are `x86_64`, `x86`, `aarch64`, `riscv64`, `loongarch64`,
`s390x` and `ppc64`.

### Validation

- The name is a DNS label and no other VM or container in the lab has it.
- `cpus` is at least 1. `memory` and `disk` are non-negative sizes.
- `disk` on a store or registry template is an error; clones inherit the
  template's disk. Use `disk "name" {}` blocks for extra disks.
- Every name in `depends_on` and `depends_on_provision` exists in the lab and is
  not the machine itself.
- Neither key forms a cycle across VMs and containers; the error names the whole
  path. The two keys are separate graphs, so a loop that crosses from one to the
  other is allowed: starting a machine never waits for provisioning, so it cannot
  deadlock.
- `profile`, if set, names a known profile.
- `cdrom` and `floppy` name files that exist under the lab root.
- `clock` is `host`, `snapshot`, or an RFC 3339 date-time such as
  `2020-01-01T00:00:00Z`.
- `secure_boot = true` is rejected when the firmware resolves to `seabios`.
  Either value may have been inherited, so the message names the layer each came
  from.
- `secure_boot_keys` (other than `microsoft`) or `secure_boot_vars` is rejected
  where secure boot resolves off, naming both layers (a store inherited from
  below the layer that turned secure boot off is fine); both on one block is
  rejected; `secure_boot_vars` must name an existing file.
- A VM with `share {}` blocks that are not `transport = "virtiofs"` needs at
  least one `nic {}`, because SMB is reachable only over a segment.
- A `playbook {}` on a VM whose arch is known to be something other than
  `x86_64` is rejected unless it names a `guest_bin`; config-weave ships guest
  binaries for x86_64 only.

```wcl
# examples/mixed-lab/vmlab.wcl
vm "winsrv" {
  template = "x86_64/windows-server-2025"
  cpus = 4
  memory = 8GiB
  nic {
    segment = "lan"
    ip = "10.70.0.10"
  }  # DHCP reservation
  share {
    host = "./shared"
    guest = "S:"
  }  # auto-mounted when ready

  # Runs once winsrv is ready; nix01 depends on it, so it waits.
  provision "scripts/setup.ws" { }
}
```

### The `@dev` decorator

`@dev` is written on the line before a `vm` or `container` block and marks it as
a dev machine (see dev-machines.md): when a workspace is named, vmlab syncs
that host directory onto it both ways. It is a decorator rather than a child block because it states something
about the machine; nothing it carries is a setting the guest sees. A bare `@dev`
is complete. Any number of machines may carry it, and zero is normal.

```wcl
@dev(default = true, workspace = "./workspace", workspace_guest = "C:\\src")
vm "dev01" { … }
```

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default` | bool | `false` | Make this the lab's default dev machine. At most one per lab. The only `@dev` machine in a lab is the default implicitly. |
| `workspace` | utf8 | none | Host directory whose contents sync into the workspace, relative to the lab root. Without it the machine is still `@dev` but has no workspace. |
| `workspace_guest` | utf8 | profile, else `/src` | Guest path the workspace lands at. Inherited from the profile (`C:\src` on Windows profiles, `/src` on Linux ones) if omitted. |

Unset arguments resolve in the order `@dev` argument, then the machine's
effective profile, then the vmlab floor of `/src`. A profile that sets no
`workspace_guest` still hosts a dev machine. The schema rejects an unknown
argument, a wrong type, a repeated `@dev`, or `@dev` on a block that is not a
`vm` or `container`. Validation adds one rule: two machines with `default = true`
is an error naming both. With more than one `@dev` machine and none declaring
`default = true`, the lab has no default; the `vmlab dev` selection ladder then
needs an argument (see dev-machines.md).

```wcl
# vmlab.wcl
@dev(default = true, workspace = "./workspace")
vm "dev01" {
  template   = "x86_64/windows-server-2025"
  cpus       = 4
  memory     = 8GiB
  depends_on = ["dc01"]
  nic { segment = "corp" }

  // The default login is the domain user, so `vmlab exec`, `vmlab shell`
  // and the workspace syncer all land on one minted logon, which is what
  // makes `dir \\dc01\team` work from a shell in the guest.
  login "dev"   { user = "PROBE\\dev"           password = "vmlab123!" default = true }
  login "admin" { user = "PROBE\\Administrator" password = "vmlab123!" }

  // Declaration order is run order. Join first, then place files into the
  // domain user's home, which at that moment does not exist yet.
  provision "scripts/join-domain.ws" { }
  provision "scripts/home-bits.ws" { }
}
```

## login {}

A labelled identity on a machine: the guest account `exec`, `shell` and the
workspace syncer run as (see logins.md). Repeatable, so one account may be
declared twice at different elevation, and `--user` or `as_login` selects
between labels.

```wcl
login "<label>" {
  user     = "PROBE\\dev"
  password = "…"
  elevated = true
  default  = false
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `label` | utf8 (label) | required | Identity label, what `--user` and `as_login` select it by, for example `dev`. Unique per machine; the inline block label. |
| `user` | utf8 | required | Guest account to log on as, for example `PROBE\dev`. |
| `password` | utf8 | none | The account's password, written plainly. Required on a Windows-family profile. |
| `elevated` | bool | `true` | Run the session elevated. Windows only; declaring it on a Linux-family profile is an error. |
| `default` | bool | implied | Make this the machine's default identity. Implied when the machine declares exactly one login. |

The family the rules are judged against is the machine's resolved profile: a
name starting with `windows` is Windows, one starting with `linux` is Linux, and
anything else, including `custom` and a registry template not yet pulled, is
unknown and gets neither family rule. A container is always Linux. Validation
enforces:

- On a Windows family, every login has a `password`; the agent runs as
  LocalSystem and there is no credential-free logon.
- On a Linux family, no login declares `elevated`; root is root, and the field
  would be read nowhere.
- No two logins on one machine share a label.
- At most one login on a machine sets `default = true`; the message names both.

The password is written plainly because the lab's own provision script created
the account with the same string. There is no credential store, no login verb
and no wscript credential API.

## nic {}

One network interface, attached to a declared segment or to the lab's built-in
NAT segment. A machine with no `nic {}` blocks has no network hardware at all.
See networking.md.

```wcl
nic {
  segment  = "corp"      // or: nat = true
  ip       = "10.50.0.10"
  gateway  = false
  mac      = "52:54:00:ab:cd:ef"
  isolated = false
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `segment` | utf8 | none | Segment name to attach to. Required unless `nat = true`. |
| `nat` | bool | `false` | Shorthand: attach to the per-lab built-in NAT segment. |
| `ip` | utf8 | dynamic | Static IPv4, which becomes a DHCP reservation. Must be in the subnet and unique. |
| `gateway` | bool | `false` | Make this NIC the segment gateway. It must own the subnet's first usable address. |
| `mac` | utf8 | generated | Fixed MAC, for example `52:54:00:ab:cd:ef`. Generated and persisted otherwise. |
| `isolated` | bool | `false` | Port isolation: reach the gateway and forwards but not segment neighbours. |

Validation enforces these rules:

- Exactly one of `segment` or `nat = true` is set. Both is an error; neither is
  an error.
- `segment` names a segment declared in the lab.
- `ip` parses as an IPv4 address, lies inside the segment's declared subnet, is
  not the network, broadcast or gateway address, and is unique across the lab. A
  static IP needs a segment with a declared `subnet`, and is not supported on the
  built-in NAT segment.
- `mac` parses as a MAC address and is unique across the lab.
- `gateway = true` needs a declared segment, a static `ip` equal to the segment's
  first usable address, a segment without `nat = true`, a segment that is not
  `global`, and no other gateway NIC on that segment.

## disk {}

An additional disk beyond the primary one: blank at a given size, or a fresh FAT
filesystem with a folder copied onto it, or both.

```wcl
disk "data" { size = 10GiB }
disk "payload" { from = "./payload/" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | Disk identifier; the inline block label. |
| `size` | ByteSize | none | Blank disk size, for example `10GiB`. One of `size` or `from` is required. |
| `from` | utf8 | none | Folder copied onto a fresh FAT filesystem. One of `size` or `from` is required. |

At least one of `size` and `from` must be set; both together is allowed. A
`from` folder must exist under the lab root.

## share {}

A host directory mounted into the guest, over virtiofs when host and guest
support it, otherwise over SMB served at the segment gateway. See
shares-media.md.

```wcl
share {
  host      = "./shared"
  guest     = "/mnt/src"
  readonly  = false
  smb1      = false
  name      = "src"
  transport = "auto"
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `host` | utf8 | required | Host directory to share. Must exist. |
| `guest` | utf8 | required | Guest mount path, for example `/mnt/src` or `D:\data`. |
| `readonly` | bool | `false` | Mount read-only. |
| `smb1` | bool | `false` | Enable the SMB1 dialect and the auth relaxation XP and 2003-era guests need. |
| `name` | utf8 | derived | Share name. Derived from the guest path if omitted. |
| `transport` | utf8 | `auto` | `auto` picks virtiofs when host and guest support it, else SMB; `virtiofs` or `smb` force one. |

The derived name joins the alphanumeric runs of the guest path with
underscores, so `/mnt/src` becomes `mnt_src` and `D:\data` becomes `d_data`.
Validation requires `host` to be a directory, a derivable or explicit `name`,
and rejects `smb1 = true` with `transport = "virtiofs"`. A VM whose shares are
not all `virtiofs` needs a NIC.

## media {}

An ISO or floppy image built from a folder and attached to the machine. Built
images are content-addressed under the lab's `.vmlab/` directory, so an
unchanged folder is not rebuilt.

```wcl
media { kind = "iso" from = "./unattend/" label = "UNATTEND" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `kind` | utf8 | required | Image kind: `iso` or `floppy`. |
| `from` | utf8 | required | Source folder built into the image. Must exist. |
| `label` | utf8 | none | Volume label for the image. |

Validation requires `from` to be a directory under the lab root.

## gpu {}

GPU acceleration for the VM. At most one per VM. Passthrough hands a host device
to the VM exclusively; virgl and vulkan render on the host GPU, which stays
shared. The GPU is a **host GPU** declared by name in the host config
(host-profiles.md, `gpu "<name>" {}`): a lab never carries a PCI address.

```wcl
gpu { mode = "vulkan" }                    // the host's default GPU
gpu { mode = "vulkan" host_gpu = "AMD1" }  // a named one
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | utf8 | required | `passthrough`, `virgl` or `vulkan`. |
| `host_gpu` | utf8 | the host's default GPU | Name of a host GPU in the host config, matched case-insensitively. The device handed over under `passthrough`; the GPU that renders under `virgl`/`vulkan`. |

The lab daemon picks at VM start: a declared `host_gpu` is used; one this host
does not declare warns and falls back to the default under `virgl`/`vulkan`
(``host GPU `X` is not declared on this host; using the default `Y` ``) but is
refused under `passthrough`, naming the declared GPUs; no `host_gpu` takes the
default. With nothing to pick (none declared, or a missing name and no default)
`virgl`/`vulkan` render where QEMU puts them and `passthrough` refuses
(``passthrough needs a host GPU: declare `gpu "<name>" { address = ... }` ``).
`vmlab status -v` shows the pick as `host_gpu=<name>`.

A paravirtual GPU is the guest's only display adapter: `virtio-vga-gl` on x86,
`virtio-gpu-gl` plus `-vga none` on `virt`; `vulkan` adds
`venus=true,blob=true,hostmem=4G`. `passthrough` emits
`-device vfio-pci,host=<address>`.

vmlab resolves the picked GPU's address through `/sys/bus/pci/devices/<address>/`:
the render node goes on `-display egl-headless,rendernode=…`, and under `vulkan`
the kernel driver sets `VK_LOADER_DRIVERS_SELECT` in QEMU's own environment
(`amdgpu` → `*radeon*`, `i915`/`xe` → `*intel*`, `nvidia` → `*nvidia*`); the
host GPU's `vulkan_icd` replaces that with `VK_DRIVER_FILES`. Under `vulkan`
QEMU also loads vmlab's implicit layer `VK_LAYER_VMLAB_pci_filter`
(`VK_ADD_IMPLICIT_LAYER_PATH=<data dir>/vk-layer/<digest>`,
`VMLAB_VK_PCI_ADDRESS=<address>`), which hides every physical device but the one
at the GPU's PCI address — so the guest sees one Venus device even when two
identical cards match the driver filter. Every QEMU starts with
`VK_DRIVER_FILES`, `VK_ICD_FILENAMES`, `VK_LOADER_DRIVERS_SELECT`,
`MESA_VK_DEVICE_SELECT` and the layer's three variables cleared;
`MESA_VK_DEVICE_SELECT` is never set (it breaks Venus). `up` and `validate` warn when a virgl/vulkan VM gets no host GPU and the
host has more than one render node. Each run's `qemu.log` opens with the QEMU
argv and the variables vmlab set.

Validation runs the same pick: it rejects an empty `host_gpu`, a `passthrough`
name the host lacks or `passthrough` with nothing to pick, a picked GPU whose
address names no PCI device (or no render node, for virgl/vulkan), and a
`vulkan_icd` that does not exist. A virgl/vulkan name the host lacks is a
warning. The v0.24 lab fields `address` and `vulkan_icd` are gone: a lab still
carrying one fails with ``gpu `address` moved to the host config: declare
`gpu "<name>" { address = "0000:01:00.0" }` in ~/.config/vmlab/config.wcl and
name it here with `host_gpu = "<name>"` (or mark it `default = true`)``.

## provision {}

A wscript provision script run on `vmlab up` once this machine is ready. It runs
once, at its position among the machine's `provision` and `playbook` blocks. See
automation.md.

```wcl
provision "scripts/setup.ws" { }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `script` | utf8 (label) | required | Path to the `.ws` file, relative to the lab root; the inline label. Must exist and compile. |

Validation checks the file exists and compiles it, reporting compile errors
against the block. Different machines provision in parallel; only
`depends_on_provision` holds one machine's first step until another machine's
steps have all finished.

## playbook {}

A config-weave playbook applied on `vmlab up`, interleaved with the machine's
provisions in declaration order, and runnable on demand with `vmlab playbook
check` and `apply`. See automation.md.

```wcl
playbook "playbooks/baseline" {
  play = "baseline"
  var "tz" { value = "UTC" }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `path` | utf8 (label) | required | Playbook folder containing `playbook.wcl`, relative to the lab root; the inline label. |
| `play` | utf8 | required | Play name inside the playbook to run. |
| `timeout` | duration | `1h` | Ceiling on each config-weave run, such as `2h` or `90min`. A run after a reboot gets the full value again. |
| `guest_dir` | utf8 | profile's `playbook_dir`, else `/var/lib/vmlab/weave` (Linux) or `C:\weave` (Windows) | Absolute guest directory the config-weave binary and the playbook folders land in. Created if missing. |
| `guest_bin` | utf8 | unset | Absolute path of a config-weave binary already in the guest. When set, vmlab pushes no binary and probes this one with `version` before each run; the playbook folder is still pushed to `guest_dir`. |
| `var {}` | children | none | Variable overrides passed to config-weave for this machine's run. |

Validation requires a non-empty `play`, a `path` that is a directory holding a
`playbook.wcl`, a `timeout` greater than zero, and no variable set twice on one
block. `guest_dir` and `guest_bin` must be absolute for the machine's guest
family: a drive-letter path on Windows, a path starting with `/` on Linux
(every container), and either where the family is unknown. On Windows
`guest_bin` must also end in `.exe`. config-weave ships guest
binaries for x86_64 only, so a playbook on a VM with another known arch is
rejected unless it names a `guest_bin`.

## var {}

One variable override for the enclosing `playbook {}`, passed to config-weave as
`--var name=value` for this machine's run only.

```wcl
var "tz" { value = "UTC" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | Variable name; must be a WCL identifier. The inline block label. |
| `value` | utf8 | required | Value, passed through verbatim. config-weave reads it as a WCL expression where it can (`3` is an int, `true` a bool) and as a string otherwise. |

Validation requires the name to be letters, digits and underscores, not starting
with a digit, because config-weave binds each override as a `let` inside the
guest.

## on {}

An event handler, declared inside `lab {}`. The handler script runs when the
named event fires; a failing handler is logged and never fatal. See
automation.md for the event list.

```wcl
on "vm.crashed" { run = "scripts/collect-dumps.ws" targets = ["dc01"] }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `event` | utf8 (label) | required | Event name to handle, for example `vm.crashed`; the inline block label. |
| `run` | utf8 | required | Path to the handler `.ws` file, relative to the lab root. Must exist and compile. |
| `targets` | list<utf8> | none | VM or container names to restrict the handler to. Empty handles every occurrence. |

Validation requires a known event name and a script that exists and compiles.
`targets` may only be set on `vm.*`, `container.*` and `snapshot.*` events; a
lab-wide event with targets is an error. A `vm.*` event may target only VMs and
a `container.*` event only containers, and every target must exist.

```wcl
# examples/ad-lab/vmlab.wcl
on "vm.crashed"    { run = "scripts/collect-dumps.ws" }
on "host.disk_low" { run = "scripts/alert.ws" }
```
