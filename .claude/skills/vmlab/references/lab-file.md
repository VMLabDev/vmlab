# The lab file and the `lab {}` block

A lab is one file, `vmlab.wcl`, written in WCL. It declares the lab's name, its
machines, its segments and the automation that runs when the lab comes up.
Everything else has a default: a lab with one VM and no network declarations
still boots, gets an address and resolves names.

## How the file is found

A lab is a directory containing a file named `vmlab.wcl`. Every lab-scoped verb
walks up from the current directory, the way `git` finds its repository, and
uses the first directory that holds one as the lab root. Relative paths in the
file resolve against that root. If no ancestor holds a `vmlab.wcl`, the verb
fails with an error naming the directory it started from.

The CLI finds the lab from the current directory this way. Within a lab, machine
verbs take a bare machine name. From anywhere else, or when several labs run at
once, address a machine as `lab/machine`. Machine names are scoped per lab, and
VMs and containers share one namespace, so `web` can be either kind but not
both.

## The import and the `lab` block

The first line of the file must be `import <vmlab.wcl>`. That line binds the
file to vmlab's schema, so the WCL parser itself rejects an unknown block or
attribute before vmlab reads anything. A file without it is not a lab file and
is rejected before anything else is read.

A lab file declares exactly one `lab "<name>" { ... }` block; a file with none,
or with two, is rejected. It may also declare `template {}` blocks beside the
lab (see templates.md).

The lab name is a DNS label, because it appears in every machine's DNS name, and
it is the lab's identity on the host (see architecture.md).

```wcl
# vmlab.wcl
import <vmlab.wcl>

lab "demo" {
  segment "lan" { nat = true }
  vm "box" {
    template = "x86_64/ubuntu-24.04"
    nic { segment = "lan" }
  }
}
```

## Value types

`utf8` is a quoted string. `bool` is `true` or `false`. `i64` is an integer.
`ByteSize` is an integer with a unit, such as `8GiB` or `512MiB`. `Duration` is
an integer with a unit, such as `10s`. `list<utf8>` is a bracketed list of
strings, such as `["dc01", "dc02"]`.

## What nests where

The `lab` block holds four kinds of child: segments, machines, event handlers,
and lab-wide DNS entries. Machines in turn hold the things that belong to one
machine. A block lives inside the thing it configures, so there is nothing to
cross-reference by name.

| Block | Nests in | What it declares |
| --- | --- | --- |
| `segment` | `lab` | A virtual L2 switch, with its subnet, DHCP, DNS, NAT, guest routes, forwards and L3 rules. See networking.md. |
| `vm` | `lab` | A VM booted from a template's linked clone, with its hardware and its children below. See vm.md. |
| `container` | `lab` | An OCI image run in a micro-VM, with env, volumes, ports and a healthcheck. See containers.md. |
| `on` | `lab` | An event handler: a wscript script bound to an event name. See automation.md. |
| `record`, `sinkhole` | `lab` or `segment` | Static DNS entries and DNS sinkholes, lab-wide or per segment. |
| `nic` | `vm`, `container`, `template` | A network interface on a segment. A machine with no `nic` blocks has no network hardware at all. |
| `share` | `vm` | A host directory mounted in the guest. See shares-media.md. |
| `disk`, `media`, `gpu` | `vm` (and `template` for disks and media) | Extra disks, ISO and floppy images built from folders, and GPU acceleration. |
| `login` | `vm`, `container` | A labelled identity a surface attaches as. See logins-and-ssh.md. |
| `provision`, `playbook` | `vm`, `container`, `template` | Configuration steps run once the machine is ready, in declaration order. See automation.md. |
| `template` | the document, beside `lab` | A buildable template definition. See templates.md. |

Configuration steps are the clearest case of the nesting rule. A `provision` or
`playbook` block is declared inside the machine it configures. That machine is
the target, so there is no `target =` field. A machine's steps run in the order
its blocks appear, once the machine is ready. Across machines they follow the
order the machine blocks appear, with `depends_on` gating when each becomes
eligible. A dependent waits for its dependency to be ready *and* for that
dependency's steps to finish.

## Connectivity is always explicit

A machine with no `nic` blocks is air-gapped. It still boots, still answers the
agent, and can still be driven with `exec` and `cp`, because the agent rides
virtio-serial rather than the network. Connectivity is a ladder climbed by
declaration: nothing, then `nic { nat = true }` for internet-only access on the
lab's built-in NAT segment, then a `nic` on a declared segment. See
networking.md.

## Validation

`vmlab validate` evaluates the file against the schema and then applies every
rule that can be checked without touching QEMU, reporting every problem it finds
in one pass. Every other verb runs the same checks first and stops before any
side effect on an error. The checks fall into two layers.

**The schema layer** is WCL's own: unknown blocks and attributes, wrong types, a
required field missing, a value outside a declared range or option set, missing
required child blocks. The schema declares these with decorators on each field.

**The semantic layer** is vmlab's, and includes:

- A template reference that is archless or malformed, or names a template not in
  the store and not a registry reference.
- A `nic` naming a segment the lab does not declare, a static `ip` outside its
  segment's subnet, or a duplicate static IP or MAC.
- A cycle in `depends_on`, or a dependency naming a machine that does not exist.
- A `provision`, `playbook` or `on` handler whose file is missing, and any
  wscript that does not compile.
- A `scratch` VM missing `arch`, `profile` or `disk`, and a `disk` size on a
  cloned VM.
- `secure_boot = true` on a VM whose firmware resolves to SeaBIOS. Either value
  may have been inherited, so the message names the layer each came from.
- A `share` that is not explicitly virtiofs, a container `volume`, or a
  container `port` on a machine with no NIC, because SMB shares, volumes and
  forwards need a segment to reach the gateway on.
- A container that neither declares `cpus` and `memory` nor names a profile
  supplying them.
- A playbook `var` whose name is not a WCL identifier, or is set twice on one
  block.
- The dev-machine rules: more than one `@dev(default = true)`, a Windows `login`
  without a `password`, `elevated` on a Linux-family profile, and more than one
  `login` with `default = true` on a machine.

Validation deliberately does not check: `@dev` on a machine whose agent cannot
serve an attach is not a validation error. The agent's features are only known
once it is running, so that failure is reported by `vmlab up` as a warning and
by an attach as a refusal (see dev-machines.md).

## How a value resolves

Most hardware fields on a `vm` block are optional. A value not set comes from
the template's recorded hardware, and a value not recorded there comes from the
guest OS profile (see host-profiles.md). The profile's defaults are the floor.
The precedence is fixed:

```text
VM block  >  template  >  profile
```

A template records its hardware when it is sealed, so a VM cloned from a
template built with `memory = 4GiB` boots with four gibibytes unless the `vm`
block says otherwise. The profile is a live layer, not frozen into the template:
editing a profile reaches every VM that still resolves a field down to it.

Two machine kinds shorten the chain. A `scratch` VM has no template, so its
chain is VM block then profile, which is why validation insists on `arch`,
`profile` and `disk`. A container has no template either: its `cpus` and
`memory` come from the block or from its profile, and the shipped `container`
profile supplies a floor of one vCPU and 256 MiB. One resolver implements this
precedence for both machine kinds, and no other surface reimplements it.

## Decorators

A decorator is written on a machine block and states something *about* the
machine rather than configuring something inside it. vmlab ships one: `@dev`,
which marks a VM or container as a dev machine that vmlab publishes as an SSH
endpoint and syncs a workspace onto. Every argument is optional. A bare `@dev`
is a complete dev machine, and unset arguments resolve `@dev` then profile then
vmlab's own floor.

`default = true` names the lab's default dev machine, which is the same for
everyone who opens the file. Which dev machine is *yours* is not a property of
the file; see dev-machines.md for where it is recorded instead. The decorator's
arguments are listed with the machine blocks in vm.md.

```wcl
# examples/dev-neovim-container/vmlab.wcl
@dev(default = true, workspace = "./workspace")
container "dev01" {
  image   = "alpine:3.22"
  profile = "container"
  // Neovim wants more than the profile's floor, and a container names its
  // own size when the profile's is not the right one.
  cpus    = 2
  memory  = 1GiB
  // `:idle` keeps the micro-VM up for attaching without running the
  // image's entrypoint — a dev container has no service to be.
  mode    = :idle
  nic { segment = "lan" }

  // The container identity floor (§19.2): the agent is root and root needs
  // no credential to become an account, so a Linux `login {}` may declare
  // the account alone. Its Windows twin cannot — every credential-free
  // route there is the one Windows OpenSSH's S4U logon already
  // disqualified, and `elevated` is a validation error on this side.
  login "dev" { user = "dev" default = true }

  provision "scripts/dev-user.ws" { }
  provision "scripts/editor-bits.ws" { }
}
```

## lab {}

The one lab a file declares. The inline label is the lab's name, which becomes
part of every guest hostname and is unique on the host.

```wcl
lab "<name>" {
  gui = false
  segment "…" { … }
  vm "…" { … }
  container "…" { … }
  record { … }
  sinkhole { … }
  on "…" { … }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | Lab name, a DNS label of at most 63 characters; the inline block label. |
| `gui` | bool | unset | Default for all VMs: open a VNC viewer on `up`. A VM's own `gui` overrides it. |
| `segment {}` | children | none | Virtual L2 network segments in this lab. |
| `vm {}` | children | none | The VMs in this lab. |
| `container {}` | children | none | OCI containers in this lab, each run in a micro-VM. |
| `on {}` | children | none | Lifecycle event handlers. A handler failure is logged, never fatal. |
| `record {}` | children | none | Lab-wide static DNS entries; wildcards allowed. |
| `sinkhole {}` | children | none | Lab-wide DNS sinkholes. |

Validation checks the name is a DNS label: letters, digits and hyphens only, not
starting or ending with a hyphen, at most 63 characters. VM and container names
share one namespace inside the lab, so a `vm` and a `container` with the same
name is an error. A lab with no machines is valid.

Segments are declared in the lab file as `segment {}` children of `lab {}`;
their fields and child blocks (`dns`, `connect`, `route`, `record`, `forward`,
`block`, `redirect`, `sinkhole`) are in networking.md.

```wcl
# examples/mixed-lab/vmlab.wcl
# A small mixed Windows/Linux lab built on the two example templates
# (examples/templates/windows-server-2025 and examples/templates/
# ubuntu-24.04 — build those first). Demonstrates a NAT'd segment, a
# static IP, boot ordering, an SMB share, a host port-forward, an OCI
# container on the same segment (§18), and a provision script driving
# both guests.
#
# vmlab up
# curl http://localhost:18080      # nginx on nix01, via the forward
# curl http://localhost:18081      # nginx in the "web" container
# vmlab down

import <vmlab.wcl>

lab "mixed-lab" {

  gui = true  # open a VNC viewer for each guest on `vmlab up`

  segment "lan" {
    subnet = "10.70.0.0/24"
    nat = true  # apt needs egress
    forward {
      host_port = 18080
      to = "nix01:80"
    }  # host → nginx
  }

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

  vm "nix01" {
    template = "x86_64/ubuntu-24.04"
    memory = 2GiB
    depends_on = ["winsrv"]
    nic {
      segment = "lan"
    }  # dynamic lease
  }

  # An OCI container on the same segment: pulled like a docker image,
  # run in a micro-VM, resolvable as web.mixed-lab.<suffix> from the VMs.
  container "web" {
    image = "nginx:1.27"
    # The micro-VM's size comes from the profile; declare `cpus`/`memory`
    # here to override it. One of the two must supply them.
    profile = "container"
    depends_on = ["nix01"]
    nic {
      segment = "lan"
    }
    port {
      host = 18081
      container = 80
    }  # host → container nginx
    healthcheck {
      command = ["curl", "-fsS", "http://localhost/"]
      interval = 5s
    }
  }

  on "vm.crashed" {
    run = "scripts/on-crash.ws"
  }
}
```
