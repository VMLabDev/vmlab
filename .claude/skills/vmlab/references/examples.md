# Example labs

The `examples/` directory of the vmlab source tree holds nine labs and a set of template definitions. Each is a working `vmlab.wcl` with its scripts, and each exists to show one part of the product. The lab files below are quoted verbatim from the repository; every one passes `vmlab validate`.

Every example runs from its own directory. Each names templates that are published on a public registry: pull them as each example shows, or build them yourself under `examples/templates`. Guest credentials are the ones each template bakes in: `Administrator` / `vmlab123!` on Windows Server 2025, `vmlab` / `vmlab` on the Linux images.

## ad-lab

Directory: `examples/ad-lab`.

A small Active Directory lab: a domain controller on a `corp` segment that hands out the DC as its DNS server and routes to a second subnet, a Windows 11 client that waits for the DC through `depends_on`, and an Ubuntu build box on its own NAT'd NIC pulled from a registry. It shows a static IP as a DHCP reservation, wave ordering, a provision, and two event handlers: a crash collector and a disk-low alert.

Needs `x86_64/windows-server-2025` and a pinned `x86_64/windows-11` in the store. Both are published (see "Published templates" in templates.md), so pull them rather than build; `examples/templates` has a build for the first only. The Ubuntu box pulls itself on `up`, so its segment needs egress.

```sh
vmlab template pull ghcr.io/vmlabdev/vmlab-templates/windows-server-2025 --arch x86_64
vmlab template pull ghcr.io/vmlabdev/vmlab-templates/windows-11 --arch x86_64
cd examples/ad-lab
vmlab validate
vmlab up
vmlab status
vmlab down
```

```wcl
# examples/ad-lab/vmlab.wcl
// Worked example: a small Active Directory lab (PRD §5). Illustrates
// segments, static IPs (DHCP reservations), dependency ordering, a
// provision script, and event handlers. Requires the referenced templates
// in the store and the scripts under scripts/.

import <vmlab.wcl>

lab "ad-lab" {

  // ANCHOR: segment-corp
  segment "corp" {
    subnet = "10.50.0.0/24"
    // Hand out the DC as DNS instead of the daemon (AD owns DNS).
    dns { server = "10.50.0.10" }
    route { dest = "10.60.0.0/24" via = "10.50.0.254" }
  }
  // ANCHOR_END: segment-corp

  segment "dmz" { }

  vm "dc01" {
    template = "x86_64/windows-server-2025"
    cpus     = 4
    memory   = 8GiB
    nic { segment = "corp" ip = "10.50.0.10" }

    // Runs once dc01 is ready. `depends_on` does not wait for it; a machine
    // whose own provisions needed the DC promoted would name dc01 in
    // `depends_on_provision`.
    provision "scripts/setup.ws" { }
  }

  vm "client01" {
    template   = "x86_64/windows-11@26100.1742"
    depends_on = ["dc01"]
    nic { segment = "corp" }
  }

  vm "buildbox" {
    template = "ghcr.io/vmlabdev/vmlab-templates/ubuntu-24.04"
    arch     = "x86_64"
    nic { nat = true }
  }

  // ANCHOR: handlers
  on "vm.crashed"    { run = "scripts/collect-dumps.ws" }
  on "host.disk_low" { run = "scripts/alert.ws" }
  // ANCHOR_END: handlers
}
```

Illustrates lab-file.md, networking.md, wscript-language.md and automation.md.

## alpine-arm64

Directory: `examples/alpine-arm64`.

One Alpine Linux guest on an aarch64 machine, on a NAT'd segment with a host forward to its SSH port. On an x86 host there is no KVM for aarch64, so the VM runs under TCG emulation and boots in minutes; the TCG warning (see troubleshooting.md) is expected here. The provision script waits for the agent and logs the guest's `uname -m`, which should print `aarch64`.

Needs `aarch64/alpine-3.23` in the store. It is published, so pull it; its definition lives in the separate `vmlab-templates` repository if you need to build it instead. The host needs `qemu-system-aarch64` and the aarch64 UEFI firmware.

```sh
vmlab template pull ghcr.io/vmlabdev/vmlab-templates/alpine-3.23 --arch aarch64
cd examples/alpine-arm64
vmlab up                          # TCG: give it a minute or two
ssh vmlab@localhost -p 12222      # password: vmlab
vmlab logs                        # the provision's uname -m line
vmlab down
```

```wcl
# examples/alpine-arm64/vmlab.wcl
// Minimal arm64 (aarch64) lab: one Alpine guest on a NAT'd segment, with a
// host port-forward to its SSH port. The guest emulates aarch64 under TCG
// on x86 hosts (no KVM), so boot is slow — give it a couple of minutes.
//
// Build the template first (from the vmlab-templates repo):
//
//   cd vmlab-templates/alpine-3.23-arm64 && vmlab template build
//
// Then, from this directory:
//
//   vmlab up
//   ssh vmlab@localhost -p 12222      # password: vmlab
//   vmlab down

import <vmlab.wcl>

lab "alpine-arm64" {

  gui = true        // open a VNC viewer on `up` (virtio-gpu framebuffer)

  segment "lan" {
    subnet = "10.80.0.0/24"
    nat    = true                                  // apk needs egress
    forward { host_port = 12222 to = "alp:22" }    // host SSH → guest 22
  }

  vm "alp" {
    template = "aarch64/alpine-3.23"
    memory   = 1GiB
    nic { segment = "lan" }                        // dynamic DHCP lease

    provision "scripts/setup.ws" { }   // runs once this VM is ready
  }
}
```

Illustrates start-here.md, networking.md and the architecture fields in vm.md.

## alpine-registry

Directory: `examples/alpine-registry`.

A one-VM lab whose `template` is an OCI registry ref rather than a store ref. There is no build or pull step: the first `vmlab up` resolves the ref's tag, pulls that version into the store if it is absent, and boots. A cached version is reused without downloading again. With no tag the ref tracks the moving `latest`; `:latest-prerelease` tracks pre-releases and `:<version>` pins one.

Needs network egress from the host for the pull; the segment declares `nat = true` so `apk` works inside the guest too. To make the next `up` pull again, remove the cached version with `vmlab template rm x86_64/alpine-3.23@<version> --force`.

```sh
cd examples/alpine-registry
vmlab up                          # first run pulls the template, then boots
vmlab status                      # TEMPLATE shows the registry ref
ssh vmlab@localhost -p 12222      # password: vmlab
vmlab down
```

```wcl
# examples/alpine-registry/vmlab.wcl
// Minimal lab that pulls its template straight from an OCI registry.
//
// Unlike the other examples, this VM's `template` is an **OCI registry ref**
// (`host/owner/[group/]name[:tag]`) rather than a local `arch/name` store ref.
// On `vmlab up`, if the template is not already in the local store it is pulled
// from the registry, then the VM boots from it (PRD §6.4). No `vmlab template
// build` or manual `pull` step is required — just:
//
//   vmlab up
//   ssh vmlab@localhost -p 12222      # password: vmlab
//   vmlab down
//
// The ref below has no tag, so it tracks the moving `:latest` (newest stable)
// of the published Alpine 3.23 package. Use `...alpine-3.23:latest-prerelease`
// to track pre-releases, or `...alpine-3.23:<version>` to pin one.

import <vmlab.wcl>

lab "alpine-registry" {

  segment "lan" {
    subnet = "10.81.0.0/24"
    nat    = true                                  // apk + the registry pull need egress
    forward { host_port = 12222 to = "alp:22" }    // host SSH → guest 22
  }

  vm "alp" {
    template = "ghcr.io/vmlabdev/vmlab-templates/alpine-3.23"   // → :latest
    arch     = "x86_64"
    memory   = 1GiB
    nic { segment = "lan" }                        // dynamic DHCP lease

    provision "scripts/setup.ws" { }   // runs once this VM is ready
  }
}
```

Illustrates templates.md and `vmlab pull` (cli-lab.md).

## dev-container

Directory: `examples/dev-container`.

The worked dev-machine example. A lab container running `alpine:3.22` in idle mode is the lab's dev machine, with `./workspace` on the host landing at `/src` in the guest and syncing both ways. The `login "dev"` block declares the account alone, with no password, because the agent in a container is root and root needs no credential to become an account. That is the container identity floor.

Its point is that a provision can write into the dev login's home before that user has ever logged on. `scripts/home-bits.ws` places `~/.profile` with `dev01.as_login("dev")`, a second handle onto the same machine whose every call lands as `dev`. Without that line the file lands root-owned. The README records the durability rule: what the provision places survives `down`/`up` and a restore, dies on `vmlab container destroy dev01`, and comes back on the next `up` because it is a declaration.

Needs nothing built. The image is pulled on the first `up`, the segment has egress for the package install, and the host needs the container guest asset (see containers.md).

```sh
cd examples/dev-container
vmlab up
vmlab status                      # dev01 ready
vmlab shell dev01                 # a shell as dev, in /src
# in that shell:
./build.sh                        # writes /src/out.txt, which appears in ./workspace
# back on the host:
vmlab dev sync status
```

```wcl
# examples/dev-container/vmlab.wcl
// Worked example (PRD §19.8): **a dev machine on a Linux container micro-VM.**
//
// A dev machine is a machine with a synced workspace. This one shows three
// things: the container identity floor for a `login {}`, provisioning into
// that login's home with `as_login` before the account has ever logged on,
// and the workspace syncer carrying edits both ways.
//
// No template to build — a container is pulled like a docker image and run
// inside a micro-VM, so it is a lab machine in every respect: same segments,
// DNS, snapshots, agent channel and workspace syncer as a VM.
//
//   vmlab up
//   vmlab shell dev01         # lands as the default login, `dev`, in /src
//   vmlab dev sync status
//
// The workspace is ./workspace on the host, landing at /src in the guest.

import <vmlab.wcl>

lab "dev-container" {

  // This segment *has* egress, and the example needs it once: the package
  // install in scripts/dev-user.ws.
  segment "lan" {
    subnet = "10.62.0.0/24"
    nat    = true
  }

  // ANCHOR: dev01
  @dev(default = true, workspace = "./workspace")
  container "dev01" {
    image   = "alpine:3.22"
    profile = "container"
    // A dev machine builds things, and a container names its own size when
    // the profile's floor is not the right one.
    cpus    = 2
    memory  = 1GiB
    // `:idle` keeps the micro-VM up without running the image's entrypoint
    // — a dev container has no service to be.
    mode    = :idle
    nic { segment = "lan" }

    // The container identity floor (§19.2): the agent is root and root needs
    // no credential to become an account, so a Linux `login {}` may declare
    // the account alone. `elevated` is a validation error on this side.
    login "dev" { user = "dev" default = true }

    provision "scripts/dev-user.ws" { }
    provision "scripts/home-bits.ws" { }
  }
  // ANCHOR_END: dev01
}
```

Illustrates dev-machines.md, logins.md, containers.md and `vmlab dev` (cli-machine.md).

## mixed-lab

Directory: `examples/mixed-lab`.

Windows Server 2025 and Ubuntu 24.04 on one NAT'd segment, plus an nginx lab container on the same segment. It exercises a static IP reservation, `depends_on` ordering across all three machines, an SMB share that appears on the Windows guest as `S:`, a segment forward to nginx on the Ubuntu box, a container `port {}` and healthcheck, a provision driving both guests, and a crash handler. With `gui = true` on the lab, `up` opens a viewer per guest.

Needs `x86_64/windows-server-2025` and `x86_64/ubuntu-24.04` in the store. Both are published, so pull them; building them from `examples/templates` is the fallback. The README explains one thing about the share: the daemon maps `S:` as SYSTEM, so an interactive user opening it sees a credential error until they run the `vmlab-shares` script vmlab drops on the desktop, once per user.

```sh
vmlab template pull ghcr.io/vmlabdev/vmlab-templates/windows-server-2025 --arch x86_64
vmlab template pull ghcr.io/vmlabdev/vmlab-templates/ubuntu-24.04 --arch x86_64
cd examples/mixed-lab
vmlab validate
vmlab up                          # winsrv, then nix01, then the web container
vmlab status
curl http://localhost:18080       # nginx on nix01, through the forward
curl http://localhost:18081       # nginx in the container
vmlab down                        # clones retained; vmlab destroy deletes them
```

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

  # ANCHOR: segment-lan
  segment "lan" {
    subnet = "10.70.0.0/24"
    nat = true  # apt needs egress
    forward {
      host_port = 18080
      to = "nix01:80"
    }  # host → nginx
  }
  # ANCHOR_END: segment-lan

  # ANCHOR: vm-winsrv
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

    # Runs once winsrv is ready. nix01 starts as soon as winsrv is ready
    # (`depends_on` is start order), so this script can wait for nix01 too.
    provision "scripts/setup.ws" { }
  }
  # ANCHOR_END: vm-winsrv

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
  # ANCHOR: container-web
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
  # ANCHOR_END: container-web

  on "vm.crashed" {
    run = "scripts/on-crash.ws"
  }
}
```

Illustrates start-here.md, networking.md, shares-media.md, containers.md and automation.md.

## peer-a

Directory: `examples/peer-a`.

Side A of a cross-instance peering pair. Its `wan` segment is a global segment with a `connect {}` block naming the other side's trunk port, so the two supervisors bridge the segment over a PSK-authenticated TCP trunk and `a1` here shares one L2 segment with `b1` over there. Both sides may declare `connect` at each other; the trunk table converges the mutual dial to a single trunk.

It pulls its Alpine template from a registry on `up`. Because the two sides address each other's `trunk_port`, they run under two supervisors: two hosts with the `connect` host changed to the other machine's address, or two users on one host, each with a different `trunk_port` and the same `psk` in their host configuration. Both supervisors run a DHCP server on the bridged segment, so leases can collide; the provision adds a deterministic secondary IP, `10.99.0.10` on this side, so the cross-trunk ping has a stable target.

```sh
cd examples/peer-a
vmlab up
vmlab exec a1 -- ping -c 3 10.99.0.20   # b1 on the other side
vmlab down
```

```wcl
# examples/peer-a/vmlab.wcl
// Cross-instance peering demo, side A (see also examples/peer-b).
//
// The "wan" segment is `global` (owned by the supervisor, shared across labs)
// and declares a `connect {}` peer: the supervisor bridges it to the other
// vmlab instance's supervisor over a PSK-authenticated TCP trunk (PRD §9.2),
// so a1 here and b1 over there share one L2 segment.
//
// Both sides may declare `connect` at each other (that is what the web UI's
// remote-vmlab node writes); the trunk table converges the mutual dial to a
// single trunk. Run the whole thing with:
//
//   just peer-demo        # two isolated vmlab instances on :7871 / :7872
//   just peer-demo-stop
//
// Caveat: BOTH supervisors run a DHCP server on the bridged segment, so
// leases can race/collide. The provision adds a deterministic secondary IP
// (10.99.0.10 here, .20 on side B) so the cross-trunk ping has a stable
// target either way.

import <vmlab.wcl>

lab "peer-a" {

  // ANCHOR: segment-wan
  segment "wan" {
    subnet = "10.99.0.0/24"
    global = true
    connect { host = "127.0.0.1:13948" }   // side B's trunk_port
  }
  // ANCHOR_END: segment-wan

  vm "a1" {
    template = "ghcr.io/vmlabdev/vmlab-templates/alpine-3.23"
    arch     = "x86_64"
    memory   = 1GiB
    nic { segment = "wan" }

    provision "scripts/setup.ws" { }   // runs once this VM is ready
  }
}
```

Illustrates global segments and trunks in networking.md and the `trunk_port` and `psk` fields in host-profiles.md.

## peer-b

Directory: `examples/peer-b`.

Side B of the same pair. It is the mirror of `peer-a`: the same global `wan` segment, a `connect` pointing back at side A's trunk port, and one Alpine VM whose provision takes `10.99.0.20` as its secondary IP. Run it under the second supervisor, with everything else as for side A.

```sh
cd examples/peer-b
vmlab up
vmlab exec b1 -- ping -c 3 10.99.0.10   # a1 on the other side
vmlab down
```

```wcl
# examples/peer-b/vmlab.wcl
// Cross-instance peering demo, side B — see examples/peer-a/vmlab.wcl for
// the full story. b1 lands on the same bridged "wan" segment as a1.

import <vmlab.wcl>

lab "peer-b" {

  segment "wan" {
    subnet = "10.99.0.0/24"
    global = true
    connect { host = "127.0.0.1:13947" }   // side A's trunk_port
  }

  vm "b1" {
    template = "ghcr.io/vmlabdev/vmlab-templates/alpine-3.23"
    arch     = "x86_64"
    memory   = 1GiB
    nic { segment = "wan" }

    provision "scripts/setup.ws" { }   // runs once this VM is ready
  }
}
```

Illustrates the same material as `peer-a`.

## riscv64-ubuntu

Directory: `examples/riscv64-ubuntu`.

One Ubuntu 24.04 guest on a riscv64 machine, on a NAT'd segment with a host forward to SSH. Like `alpine-arm64` it runs under TCG on an x86 host and boots slowly by design. The provision logs `uname -m`, which should print `riscv64`.

Needs `riscv64/ubuntu-24.04` in the store, which is published (pull it as below; the separate `vmlab-templates` repository has its build), and on the host `qemu-system-riscv64` from QEMU 8.1 or later plus the riscv64 UEFI firmware, `qemu-efi-riscv64` on Debian and Ubuntu.

```sh
vmlab template pull ghcr.io/vmlabdev/vmlab-templates/ubuntu-24.04 --arch riscv64
cd examples/riscv64-ubuntu
vmlab up                          # TCG: give it a minute or two
ssh vmlab@localhost -p 12322      # password: vmlab
vmlab down
```

```wcl
# examples/riscv64-ubuntu/vmlab.wcl
// Minimal riscv64 lab: one Ubuntu guest on a NAT'd segment, with a host
// port-forward to its SSH port. The guest emulates riscv64 under TCG on x86
// hosts (no KVM), so boot is slow — give it a couple of minutes.
//
// Host needs qemu-system-riscv64 (QEMU >= 8.1) and the riscv64 UEFI firmware
// (Debian/Ubuntu package qemu-efi-riscv64, or edk2's RISCV_VIRT_*.fd).
//
// Build the template first (from the vmlab-templates repo):
//
//   cd vmlab-templates/ubuntu-riscv64 && vmlab template build
//
// Then, from this directory:
//
//   vmlab up
//   ssh vmlab@localhost -p 12322      # password: vmlab
//   vmlab down

import <vmlab.wcl>

lab "riscv64-ubuntu" {

  gui = true        // open a VNC viewer on `up` (virtio-gpu framebuffer)

  segment "lan" {
    subnet = "10.81.0.0/24"
    nat    = true                                  // apt needs egress
    forward { host_port = 12322 to = "ubu:22" }    // host SSH → guest 22
  }

  vm "ubu" {
    template = "riscv64/ubuntu-24.04"
    memory   = 2GiB
    nic { segment = "lan" }                        // dynamic DHCP lease

    provision "scripts/setup.ws" { }   // runs once this VM is ready
  }
}
```

Illustrates networking.md and the architecture and firmware handling in host-profiles.md.

## templates

Directory: `examples/templates`.

Not a lab but eight `template {}` definitions, one per directory, each building an `x86_64` template into the store with `vmlab template build` run from that directory. Seven are Linux cloud images with a `cloudinit/` folder attached as a `CIDATA` media volume: cloud-init creates the `vmlab` user, installs the guest agent from the auto-attached VMLAB ISO, and disables itself for later boots. The script waits for the agent, which is the proof the install landed, and vmlab seals the disk. Ubuntu 24.04 uses the live-server ISO with a subiquity autoinstall instead, answering the installer's confirmation prompt through OCR and keystrokes. Windows Server 2025 is fully unattended from the Evaluation Center ISO through `autounattend.xml`, after `fetch-deps.sh` stages the virtio drivers and guest MSIs.

| Directory | Builds | Source |
| --- | --- | --- |
| `almalinux-10` | `x86_64/almalinux-10` | GenericCloud image, cloud-init. Needs an x86-64-v3 host CPU. |
| `arch` | `x86_64/arch` | Dated cloud image, cloud-init. |
| `fedora-44` | `x86_64/fedora-44` | Cloud Base image, cloud-init. |
| `opensuse-leap-16.0` | `x86_64/opensuse-leap` | Minimal-VM cloud image, cloud-init. |
| `opensuse-tumbleweed` | `x86_64/opensuse-tumbleweed` | Dated Cloud-Snapshot image, cloud-init. |
| `ubuntu-24.04` | `x86_64/ubuntu-24.04` | Live-server ISO, subiquity autoinstall. |
| `ubuntu-26.04` | `x86_64/ubuntu-26.04` | Cloud image, cloud-init. |
| `windows-server-2025` | `x86_64/windows-server-2025` | Evaluation ISO, `autounattend.xml`; 30 to 45 minutes; 180-day licence. |

Every one of these is also published prebuilt (see "Published templates" in templates.md); build from here to change one, not to get one.

```sh
cd examples/templates/ubuntu-24.04
vmlab validate
vmlab template build
vmlab template list               # x86_64/ubuntu-24.04@24.04.4
```

`examples/templates/ubuntu-24.04/vmlab.wcl` is quoted in full in start-here.md ("The template definition"). The Windows one:

```wcl
# examples/templates/windows-server-2025/vmlab.wcl
// Windows Server 2025 (Evaluation) template (PRD §6.1). Fully unattended:
// unattend/ is packed into an ISO carrying autounattend.xml, the virtio
// storage/net drivers Windows Setup needs (the windows-server profile uses
// virtio disk + NIC), and the guest tooling MSIs installed on first logon.
//
//   ./fetch-deps.sh        # one-time: pull virtio drivers + MSIs into unattend/
//   vmlab template build   # ~30-45 minutes
//
// The eval ISO is licensed for 180 days of evaluation — see README.md.

import <vmlab.wcl>

template "windows-server-2025" {
  arch    = "x86_64"
  version = "26100.1742"
  profile = "windows-server"
  cpus    = 4
  memory  = 8GiB
  disk    = 60GiB

  source "iso" {
    url    = "https://software-static.download.prss.microsoft.com/dbazure/888969d5-f34g-4e03-ac9d-1f9786c66749/26100.1742.240906-0331.ge_release_svc_refresh_SERVER_EVAL_x64FRE_en-us.iso"
    sha256 = "d0ef4502e350e3c6c53c15b1b3020d38a5ded011bf04998e950720ac8579b23d"
  }

  media { kind = "iso" from = "./unattend/" label = "UNATTEND" }
  nic { nat = true }

  provision "scripts/install.ws" { }
}
```

Each README says how to bump the version: update `version`, the image file name in `url` and `sha256` from the checksum file beside the image. The cloud images boot through SeaBIOS, which each file sets with `firmware = "seabios"` on the `linux-modern` profile.

Illustrates templates.md, snapshots-vision.md and cli-template.md.

## winsrv-desktop

Directory: `examples/winsrv-desktop`.

The smallest useful lab: one Windows Server 2025 VM on a NAT'd segment with its display surfaced on the host. Every VM runs headless and serves VNC on a Unix socket; `gui = true` makes `up` launch a viewer against that socket, and `vmlab console winsrv` attaches one at any time. Closing the viewer only disconnects, because the viewer is a separate process. On WSL 2, `vmlab console --tcp` bridges the socket to a localhost port for a Windows-side client.

Needs `x86_64/windows-server-2025` in the store, which is published (pull it as below, or build it from `examples/templates`), and a viewer: the `viewer` field in host config, else `remote-viewer`, `gvncviewer` or `vncviewer` on `PATH`.

```sh
vmlab template pull ghcr.io/vmlabdev/vmlab-templates/windows-server-2025 --arch x86_64
cd examples/winsrv-desktop
vmlab validate
vmlab up                          # a viewer opens on the desktop
vmlab status                      # winsrv ready at 10.80.0.10
vmlab console winsrv              # reattach after closing the window
vmlab down
```

```wcl
# examples/winsrv-desktop/vmlab.wcl
// Minimal worked example: a single Windows Server 2025 VM whose desktop
// you actually watch. Built on examples/templates/windows-server-2025
// (build that first). The point here is the display surface (PRD §11):
//
//   gui = true     opens a VNC viewer for the guest on `vmlab up`. The VM
//                  itself always runs headless, so closing the viewer just
//                  disconnects — the desktop keeps running.
//
//   vmlab console winsrv      attach a viewer at any time (also the WSL2
//                             TCP-bridge path with --tcp).
//
//   vmlab up
//   vmlab console winsrv      # reattach after closing the window
//   vmlab down

import <vmlab.wcl>

lab "winsrv-desktop" {

  gui = true        // open a VNC viewer for each guest on `vmlab up`

  segment "lan" {
    subnet = "10.80.0.0/24"
    nat    = true                   // so the desktop can reach the internet
  }

  vm "winsrv" {
    template = "x86_64/windows-server-2025"
    cpus     = 4
    memory   = 8GiB
    nic { segment = "lan" ip = "10.80.0.10" }
    // gui = true is inherited from the lab; set `gui = false` here to keep
    // this one VM headless while still reachable via `vmlab console`.
  }
}
```

Illustrates snapshots-vision.md, `vmlab console` (cli-machine.md) and host-profiles.md.
