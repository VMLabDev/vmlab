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
  template    = "<arch>/<name>[@<version>]"   // or "scratch", or a registry ref
  arch        = "x86_64"
  profile     = "linux-modern"
  cpus        = 2
  memory      = 4GiB
  disk        = 64GiB          // scratch VMs only
  cdrom       = "./isos/boot.iso"
  floppy      = "./boot.img"
  depends_on  = ["dc01"]
  nested      = false
  gui         = false
  display     = "virtio-vga"
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
| `template` | utf8 | required | `<arch>/<name>[@<version>]` from the store, `scratch`, or an OCI registry reference. |
| `arch` | utf8 | from template | Architecture. Required for `scratch` and for registry references. |
| `profile` | utf8 | from template | Guest OS profile supplying hardware defaults. Required for `scratch`. |
| `cpus` | i64 | inherited | vCPU count, greater than 0. Inherited from template, then profile, if omitted. |
| `memory` | ByteSize | inherited | RAM as a byte size, for example `8GiB` or `512MiB`. Inherited if omitted. |
| `disk` | ByteSize | none | Primary disk size, for example `64GiB`. Scratch VMs only; rejected on cloned VMs. |
| `cdrom` | utf8 | none | Path to an ISO to attach as a CD-ROM, relative to the lab root. |
| `floppy` | utf8 | none | Path to a floppy image to attach, relative to the lab root. |
| `depends_on` | list<utf8> | none | VM or container names to wait for before this one starts. No cycles. |
| `nested` | bool | `false` | Enable nested virtualisation, which passes the host CPU through. |
| `gui` | bool | lab `gui` | Open a VNC viewer on `up`. The VM always runs headless. |
| `display` | utf8 | inherited | QEMU display device string. Inherited from template, then profile. |
| `firmware` | utf8 | inherited | Firmware: `ovmf` or `seabios`. Inherited from template, then profile. |
| `tpm` | bool | inherited | Enable a TPM 2.0 device. Inherited from template, then profile. |
| `secure_boot` | bool | inherited | Enable secure boot; OVMF only. Inherited from template, then profile. |
| `qemu_args` | list<utf8> | none | Raw QEMU flags appended last. The escape hatch; they win over everything vmlab generates. |
| `gpu {}` | child | none | GPU acceleration: passthrough, virgl or vulkan. |
| `nic {}` | children | none | Network interfaces. No NICs means air-gapped. Shares need at least one. |
| `disk {}` | children | none | Additional disks beyond the primary disk. |
| `share {}` | children | none | Shared folders over virtiofs or SMB. SMB shares require at least one NIC. |
| `media {}` | children | none | ISO or floppy images built from a folder. |
| `login {}` | children | none | Identities a surface attaches to this VM as. Without one every verb keeps the agent identity. |
| `provision {}` | children | none | wscript scripts run on `vmlab up` once this VM is ready, interleaved with its playbooks in declaration order. |
| `playbook {}` | children | none | config-weave playbooks applied on `vmlab up`, interleaved with its provisions in declaration order. |

### Template references

The `template` value takes one of three forms, and the form decides what else
the block must say.

- **Store reference** `<arch>/<name>[@<version>]`, such as
  `x86_64/windows-11@26100.1`. The arch is always written; a bare `windows-11`
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
- Every name in `depends_on` exists in the lab, and the dependency graph across
  VMs and containers has no cycle.
- `profile`, if set, names a known profile.
- `cdrom` and `floppy` name files that exist under the lab root.
- `secure_boot = true` is rejected when the firmware resolves to `seabios`.
  Either value may have been inherited, so the message names the layer each came
  from.
- A VM with `share {}` blocks that are not `transport = "virtiofs"` needs at
  least one `nic {}`, because SMB is reachable only over a segment.
- A `playbook {}` on a VM whose arch is known to be something other than
  `x86_64` is rejected; config-weave ships guest binaries for x86_64 only.

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
a dev machine (see dev-machines.md): vmlab publishes it as an SSH endpoint an
editor attaches into and, when a workspace is named, syncs that directory onto
it. It is a decorator rather than a child block because it states something
about the machine; nothing it carries is a setting the guest sees. A bare `@dev`
is complete. Any number of machines may carry it, and zero is normal.

```wcl
@dev(default = true, workspace = "./workspace", workspace_guest = "C:\\src")
vm "dev01" { … }
```

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default` | bool | `false` | Make this the lab's default dev machine. At most one per lab. The only `@dev` machine in a lab is the default implicitly. |
| `workspace` | utf8 | none | Host directory whose contents sync into the workspace, relative to the lab root. Without it the machine is attachable but has no workspace. |
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
# examples/dev-vscode-windows/vmlab.wcl
@dev(default = true, workspace = "./workspace")
vm "dev01" {
  template   = "x86_64/windows-server-2025"
  cpus       = 4
  memory     = 8GiB
  depends_on = ["dc01"]
  nic { segment = "corp" }

  // Who a surface attaches as (§19.2). The default login is the domain
  // user, so `vmlab dev attach`, the SSH facade's shell, its sftp
  // subsystem and `vmlab exec` all land on **one** minted logon — which is
  // what makes `dir \\dc01\team` work from the editor's terminal.
  //
  // The secret is written plainly because the account exists only because
  // scripts/domain.ws created it, with the same string, six lines away.
  login "dev"   { user = "PROBE\\dev"           password = "vmlab123!" default = true }
  login "admin" { user = "PROBE\\Administrator" password = "vmlab123!" }

  // Declaration order is run order. Join first, then place the editor bits
  // into the domain user's home — which at that moment does not exist yet.
  provision "scripts/join-domain.ws" { }
  provision "scripts/editor-bits.ws" { }
}
```

## login {}

A labelled identity on a machine: the guest account a surface attaches as (see
logins-and-ssh.md). Repeatable, so one account may be declared twice at
different elevation, and an SSH username selects between labels.

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
| `label` | utf8 (label) | required | Identity label, what an SSH username selects it by, for example `dev`. Unique per machine; the inline block label. |
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
shared.

```wcl
gpu { mode = "passthrough" address = "0000:01:00.0" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | utf8 | required | `passthrough`, `virgl` or `vulkan`. |
| `address` | utf8 | none | Host PCI address, for example `0000:01:00.0`. Required for `passthrough`. |

Validation rejects `passthrough` without an `address`.

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
against the block. Across machines, steps follow the order the machine blocks
appear, with `depends_on` gating when each becomes eligible.

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
| `var {}` | children | none | Variable overrides passed to config-weave for this machine's run. |

Validation requires a non-empty `play`, a `path` that is a directory holding a
`playbook.wcl`, and no variable set twice on one block. config-weave ships guest
binaries for x86_64 only, so a playbook on a VM with another known arch is
rejected.

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
